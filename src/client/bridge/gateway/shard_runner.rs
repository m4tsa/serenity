use crate::gateway::{InterMessage, ReconnectType, Shard, ShardAction};
use crate::internal::prelude::*;
use crate::internal::ws_impl::{ReceiverExt, SenderExt};
use crate::internal::AsyncRwLock;
use crate::model::event::{Event, GatewayEvent};
use crate::CacheAndHttp;
use futures::lock::Mutex;
use serde::Deserialize;
use std::{
    borrow::Cow,
    sync::Arc,
};
use super::super::super::dispatch::{DispatchEvent, dispatch};
use super::super::super::{EventHandler, RawEventHandler};
use super::event::{ClientEvent, ShardStageUpdateEvent};
use super::{ShardClientMessage, ShardId, ShardManagerMessage, ShardRunnerMessage};
use tungstenite::{
    error::Error as TungsteniteError,
    protocol::frame::CloseFrame,
};
use typemap::ShareMap;

#[cfg(feature = "framework")]
use crate::framework::Framework;
#[cfg(feature = "voice")]
use super::super::voice::ClientVoiceManager;
use log::{error, debug, warn};

use futures::channel::mpsc::{UnboundedSender, UnboundedReceiver, unbounded};
use futures::stream::StreamExt;
use futures::sink::SinkExt;
use std::time::Duration;
use tokio::time::timeout;

/// A runner for managing a [`Shard`] and its respective WebSocket client.
///
/// [`Shard`]: ../../../gateway/struct.Shard.html
pub struct ShardRunner {
    data: Arc<AsyncRwLock<ShareMap>>,
    event_handler: Option<Arc<dyn EventHandler>>,
    raw_event_handler: Option<Arc<dyn RawEventHandler>>,
    #[cfg(feature = "framework")]
    framework: Arc<Mutex<Option<Box<dyn Framework + Send>>>>,
    manager_tx: UnboundedSender<ShardManagerMessage>,
    // channel to receive messages from the shard manager and dispatches
    runner_rx: UnboundedReceiver<InterMessage>,
    // channel to send messages to the shard runner from the shard manager
    runner_tx: UnboundedSender<InterMessage>,
    shard: Shard,
    #[cfg(feature = "voice")]
    voice_manager: Arc<Mutex<ClientVoiceManager>>,
    cache_and_http: Arc<CacheAndHttp>,
}

impl ShardRunner {
    /// Creates a new runner for a Shard.
    pub fn new(opt: ShardRunnerOptions) -> Self {
        let (tx, rx) = unbounded();

        Self {
            runner_rx: rx,
            runner_tx: tx,
            data: opt.data,
            event_handler: opt.event_handler,
            raw_event_handler: opt.raw_event_handler,
            #[cfg(feature = "framework")]
            framework: opt.framework,
            manager_tx: opt.manager_tx,
            shard: opt.shard,
            #[cfg(feature = "voice")]
            voice_manager: opt.voice_manager,
            cache_and_http: opt.cache_and_http,
        }
    }

    /// Starts the runner's loop to receive events.
    ///
    /// This runs a loop that performs the following in each iteration:
    ///
    /// 1. checks the receiver for [`ShardRunnerMessage`]s, possibly from the
    /// [`ShardManager`], and if there is one, acts on it.
    ///
    /// 2. checks if a heartbeat should be sent to the discord Gateway, and if
    /// so, sends one.
    ///
    /// 3. attempts to retrieve a message from the WebSocket, processing it into
    /// a [`GatewayEvent`]. This will block for 100ms before assuming there is
    /// no message available.
    ///
    /// 4. Checks with the [`Shard`] to determine if the gateway event is
    /// specifying an action to take (e.g. resuming, reconnecting, heartbeating)
    /// and then performs that action, if any.
    ///
    /// 5. Dispatches the event via the Client.
    ///
    /// 6. Go back to 1.
    ///
    /// [`GatewayEvent`]: ../../../model/event/enum.GatewayEvent.html
    /// [`Shard`]: ../../../gateway/struct.Shard.html
    /// [`ShardManager`]: struct.ShardManager.html
    /// [`ShardRunnerMessage`]: enum.ShardRunnerMessage.html
    pub async fn run(&mut self) -> Result<()> {
        debug!("[ShardRunner {:?}] Running", self.shard.shard_info());

        loop {
            if !self.recv().await? {
                return Ok(());
            }

            // check heartbeat
            if !self.shard.check_heartbeat() {
                warn!(
                    "[ShardRunner {:?}] Error heartbeating",
                    self.shard.shard_info(),
                );

                return self.request_restart().await;
            }

            let pre = self.shard.stage();
            let (event, action, successful) = self.recv_event().await;
            let post = self.shard.stage();

            if post != pre {
                self.update_manager().await;

                let e = ClientEvent::ShardStageUpdate(ShardStageUpdateEvent {
                    new: post,
                    old: pre,
                    shard_id: ShardId(self.shard.shard_info()[0]),
                });
                self.dispatch(DispatchEvent::Client(e)).await;
            }

            match action {
                Some(ShardAction::Reconnect(ReconnectType::Reidentify)) => {
                    return self.request_restart().await
                },
                Some(other) => {
                    let _ = self.action(&other).await;
                },
                None => {},
            }

            if let Some(event) = event {
                self.dispatch(DispatchEvent::Model(event)).await;
            }

            if !successful && !self.shard.stage().is_connecting() {
                return self.request_restart().await;
            }
        }
    }

    /// Clones the internal copy of the Sender to the shard runner.
    pub(super) fn runner_tx(&self) -> UnboundedSender<InterMessage> {
        self.runner_tx.clone()
    }

    /// Takes an action that a [`Shard`] has determined should happen and then
    /// does it.
    ///
    /// For example, if the shard says that an Identify message needs to be
    /// sent, this will do that.
    ///
    /// # Errors
    ///
    /// Returns
    async fn action(&mut self, action: &ShardAction) -> Result<()> {
        match *action {
            ShardAction::Reconnect(ReconnectType::Reidentify) => {
                self.request_restart().await
            },
            ShardAction::Reconnect(ReconnectType::Resume) => {
                self.shard.resume().await
            },
            ShardAction::Reconnect(ReconnectType::__Nonexhaustive) => unreachable!(),
            ShardAction::Heartbeat => self.shard.heartbeat(),
            ShardAction::Identify => self.shard.identify(),
            ShardAction::__Nonexhaustive => unreachable!(),
        }
    }

    // Checks if the ID received to shutdown is equivalent to the ID of the
    // shard this runner is responsible. If so, it shuts down the WebSocket
    // client.
    //
    // Returns whether the WebSocket client is still active.
    //
    // If true, the WebSocket client was _not_ shutdown. If false, it was.
    async fn checked_shutdown(&mut self, id: ShardId) -> bool {
        // First verify the ID so we know for certain this runner is
        // to shutdown.
        if id.0 != self.shard.shard_info()[0] {
            // Not meant for this runner for some reason, don't
            // shutdown.
            return true;
        }

        // Send a Close Frame to Discord, which allows a bot to "log off"
        let _ = self.shard.client.close(Some(CloseFrame {
            code: 1000.into(),
            reason: Cow::from(""),
        }));

        // In return, we wait for either a Close Frame response, or an error, after which this WS is deemed
        // disconnected from Discord.
        loop {
            match self.shard.client.read_message() {
                Ok(tungstenite::Message::Close(_)) => break,
                Err(_) => {
                    warn!(
                        "[ShardRunner {:?}] Received an error awaiting close frame",
                        self.shard.shard_info(),
                    );
                    break;
                }
                _ => continue,
            }
        }

        // Inform the manager that shutdown for this shard has finished.
        if let Err(why) = self.manager_tx.send(ShardManagerMessage::ShutdownFinished(id)).await {
            warn!(
                "[ShardRunner {:?}] Could not send ShutdownFinished: {:#?}",
                self.shard.shard_info(),
                why,
            );
        }
        false
    }

    #[inline]
    async fn dispatch(&self, event: DispatchEvent) {
        dispatch(
            event,
            #[cfg(feature = "framework")]
            &self.framework,
            &self.data,
            &self.event_handler,
            &self.raw_event_handler,
            &self.runner_tx,
            self.shard.shard_info()[0],
            Arc::clone(&self.cache_and_http),
        ).await;
    }

    // Handles a received value over the shard runner rx channel.
    //
    // Returns a boolean on whether the shard runner can continue.
    //
    // This always returns true, except in the case that the shard manager asked
    // the runner to shutdown.
    async fn handle_rx_value(&mut self, value: InterMessage) -> bool {
        match value {
            InterMessage::Client(value) => match *value {
                    ShardClientMessage::Manager(ShardManagerMessage::Start(_,_)) => {
                        true
                    },
                    ShardClientMessage::Manager(ShardManagerMessage::Restart(id)) |
                    ShardClientMessage::Manager(ShardManagerMessage::Shutdown(id)) => {
                        self.checked_shutdown(id).await
                    },
                    ShardClientMessage::Manager(ShardManagerMessage::ShutdownAll) => {
                        // This variant should never be received.
                        warn!(
                            "[ShardRunner {:?}] Received a ShutdownAll?",
                            self.shard.shard_info(),
                        );

                        true
                    },
                    ShardClientMessage::Manager(ShardManagerMessage::ShardUpdate { .. }) => {
                        // nb: not sent here

                        true
                    },
                    ShardClientMessage::Manager(ShardManagerMessage::ShutdownInitiated) => {
                        // nb: not sent here

                        true
                    },
                    ShardClientMessage::Manager(ShardManagerMessage::ShutdownFinished(_)) => {
                        // nb: not sent here

                        true
                    },
                ShardClientMessage::Runner(ShardRunnerMessage::ChunkGuilds { guild_ids, limit, query }) => {
                    self.shard.chunk_guilds(
                        guild_ids,
                        limit,
                        query.as_ref().map(String::as_str),
                    ).is_ok()
                },
                ShardClientMessage::Runner(ShardRunnerMessage::Close(code, reason)) => {
                    let reason = reason.unwrap_or_else(String::new);
                    let close = CloseFrame {
                        code: code.into(),
                        reason: Cow::from(reason),
                    };
                    self.shard.client.close(Some(close)).is_ok()
                },
                ShardClientMessage::Runner(ShardRunnerMessage::Message(msg)) => {
                    self.shard.client.write_message(msg).is_ok()
                },
                ShardClientMessage::Runner(ShardRunnerMessage::SetActivity(activity)) => {
                    // To avoid a clone of `activity`, we do a little bit of
                    // trickery here:
                    //
                    // First, we obtain a reference to the current presence of
                    // the shard, and create a new presence tuple of the new
                    // activity we received over the channel as well as the
                    // online status that the shard already had.
                    //
                    // We then (attempt to) send the websocket message with the
                    // status update, expressively returning:
                    //
                    // - whether the message successfully sent
                    // - the original activity we received over the channel
                    self.shard.set_activity(activity);

                    self.shard.update_presence().is_ok()
                },
                ShardClientMessage::Runner(ShardRunnerMessage::SetPresence(status, activity)) => {
                    self.shard.set_presence(status, activity);

                    self.shard.update_presence().is_ok()
                },
                ShardClientMessage::Runner(ShardRunnerMessage::SetStatus(status)) => {
                    self.shard.set_status(status);

                    self.shard.update_presence().is_ok()
                },
            },
            InterMessage::Json(value) => {
                // Value must be forwarded over the websocket
                self.shard.client.send_json(&value).is_ok()
            },
            InterMessage::__Nonexhaustive => unreachable!(),
        }
    }

    #[cfg(feature = "voice")]
    fn handle_voice_event(&self, event: &Event) {
        match *event {
            Event::Ready(_) => {
                self.voice_manager.lock().set(
                    self.shard.shard_info()[0],
                    self.runner_tx.clone(),
                );
            },
            Event::VoiceServerUpdate(ref event) => {
                if let Some(guild_id) = event.guild_id {
                    let mut manager = self.voice_manager.lock();
                    let search = manager.get_mut(guild_id);

                    if let Some(handler) = search {
                        handler.update_server(&event.endpoint, &event.token);
                    }
                }
            },
            Event::VoiceStateUpdate(ref event) => {
                if let Some(guild_id) = event.guild_id {
                    let mut manager = self.voice_manager.lock();
                    let search = manager.get_mut(guild_id);

                    if let Some(handler) = search {
                        handler.update_state(&event.voice_state);
                    }
                }
            },
            _ => {},
        }
    }

    // Receives values over the internal shard runner rx channel and handles
    // them.
    //
    // This will loop over values until there is no longer one.
    //
    // Requests a restart if the sending half of the channel disconnects. This
    // should _never_ happen, as the sending half is kept on the runner.

    // Returns whether the shard runner is in a state that can continue.
    async fn recv(&mut self) -> Result<bool> {
        // TODO: we use a 50 ms timeout here to simulate a non-blocking call
        let next =
            timeout(Duration::from_millis(50), self.runner_rx.next());

        let mut _value = None;

        if let Ok(Some(value)) = next.await {
            _value = Some(value);
        }

        if let Some(value) = _value {
            if !self.handle_rx_value(value).await {
                return Ok(false);
            }
        }


        Ok(true)
    }

    /// Returns a received event, as well as whether reading the potentially
    /// present event was successful.
    async fn recv_event(&mut self) -> (Option<Event>, Option<ShardAction>, bool) {
        let gw_event = match self.shard.client.recv_json() {
            Ok(Some(value)) => {
                GatewayEvent::deserialize(value).map(Some).map_err(From::from)
            },
            Ok(None) => Ok(None),
            Err(Error::Tungstenite(TungsteniteError::Io(_))) => {
                // Check that an amount of time at least double the
                // heartbeat_interval has passed.
                //
                // If not, continue on trying to receive messages.
                //
                // If it has, attempt to auto-reconnect.
                {
                    let last = self.shard.last_heartbeat_ack();
                    let interval = self.shard.heartbeat_interval();

                    if let (Some(last_heartbeat_ack), Some(interval)) = (last, interval) {
                        let seconds_passed = last_heartbeat_ack.elapsed().as_secs();
                        let interval_in_secs = interval / 1000;

                        if seconds_passed <= interval_in_secs * 2 {
                            return (None, None, true);
                        }
                    } else {
                        return (None, None, true);
                    }
                }

                debug!("Attempting to auto-reconnect");

                match self.shard.reconnection_type() {
                    ReconnectType::Reidentify => return (None, None, false),
                    ReconnectType::Resume => {
                        if let Err(why) = self.shard.resume().await {
                            warn!("Failed to resume: {:?}", why);

                            return (None, None, false);
                        }
                    },
                    ReconnectType::__Nonexhaustive => unreachable!(),
                }

                return (None, None, true);
            },
            Err(why) => Err(why),
        };

        let event = match gw_event {
            Ok(Some(event)) => Ok(event),
            Ok(None) => return (None, None, true),
            Err(why) => Err(why),
        };

        let action = match self.shard.handle_event(&event) {
            Ok(Some(action)) => Some(action),
            Ok(None) => None,
            Err(why) => {
                error!("Shard handler received err: {:?}", why);

                return (None, None, true);
            },
        };

        if let Ok(GatewayEvent::HeartbeatAck) = event {
            self.update_manager().await;
        }

        #[cfg(feature = "voice")]
        {
            if let Ok(GatewayEvent::Dispatch(_, ref event)) = event {
                self.handle_voice_event(&event);
            }
        }

        let event = match event {
            Ok(GatewayEvent::Dispatch(_, event)) => Some(event),
            _ => None,
        };

        (event, action, true)
    }

    async fn request_restart(&mut self) -> Result<()> {
        self.update_manager().await;

        debug!(
            "[ShardRunner {:?}] Requesting restart",
            self.shard.shard_info(),
        );
        let shard_id = ShardId(self.shard.shard_info()[0]);
        let msg = ShardManagerMessage::Restart(shard_id);
        let _ = self.manager_tx.send(msg).await;

        #[cfg(feature = "voice")]
        {
            self.voice_manager.lock().manager_remove(shard_id.0);
        }

        Ok(())
    }

    async fn update_manager(&mut self) {
        let _ = self.manager_tx.send(ShardManagerMessage::ShardUpdate {
            id: ShardId(self.shard.shard_info()[0]),
            latency: self.shard.latency(),
            stage: self.shard.stage(),
        }).await;
    }
}

/// Options to be passed to [`ShardRunner::new`].
///
/// [`ShardRunner::new`]: struct.ShardRunner.html#method.new
pub struct ShardRunnerOptions {
    pub data: Arc<AsyncRwLock<ShareMap>>,
    pub event_handler: Option<Arc<dyn EventHandler>>,
    pub raw_event_handler: Option<Arc<dyn RawEventHandler>>,
    #[cfg(feature = "framework")]
    pub framework: Arc<Mutex<Option<Box<dyn Framework + Send>>>>,
    pub manager_tx: UnboundedSender<ShardManagerMessage>,
    pub shard: Shard,
    #[cfg(feature = "voice")]
    pub voice_manager: Arc<Mutex<ClientVoiceManager>>,
    pub cache_and_http: Arc<CacheAndHttp>,
}

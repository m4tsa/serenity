use crate::gateway::Shard;
use crate::internal::prelude::*;
use crate::internal::AsyncRwLock;
use crate::CacheAndHttp;
use futures::lock::Mutex;
use std::{
    collections::VecDeque,
    sync::{
        Arc
    },
    time::{Duration, Instant}
};
use super::super::super::{EventHandler, RawEventHandler};
use super::{
    ShardId,
    ShardManagerMessage,
    ShardQueuerMessage,
    ShardRunner,
    ShardRunnerInfo,
    ShardRunnerOptions,
};
use typemap::ShareMap;
use crate::gateway::ConnectionStage;
use log::{info, warn};

#[cfg(feature = "voice")]
use crate::client::bridge::voice::ClientVoiceManager;
#[cfg(feature = "framework")]
use crate::framework::Framework;

use tokio::time::delay_for;
use futures::channel::mpsc::{UnboundedSender, UnboundedReceiver};
use futures::stream::StreamExt;

const WAIT_BETWEEN_BOOTS_IN_SECONDS: u64 = 5;

/// The shard queuer is a simple loop that runs indefinitely to manage the
/// startup of shards.
///
/// A shard queuer instance _should_ be run in its own thread, due to the
/// blocking nature of the loop itself as well as a 5 second thread sleep
/// between shard starts.
pub struct ShardQueuer {
    /// A copy of [`Client::data`] to be given to runners for contextual
    /// dispatching.
    ///
    /// [`Client::data`]: ../../struct.Client.html#structfield.data
    pub data: Arc<AsyncRwLock<ShareMap>>,
    /// A reference to an `EventHandler`, such as the one given to the
    /// [`Client`].
    ///
    /// [`Client`]: ../../struct.Client.html
    pub event_handler: Option<Arc<dyn EventHandler>>,
    /// A reference to an `RawEventHandler`, such as the one given to the
    /// [`Client`].
    ///
    /// [`Client`]: ../../struct.Client.html
    pub raw_event_handler: Option<Arc<dyn RawEventHandler>>,
    /// A copy of the framework
    #[cfg(feature = "framework")]
    pub framework: Arc<Mutex<Option<Box<dyn Framework + Send>>>>,
    /// The instant that a shard was last started.
    ///
    /// This is used to determine how long to wait between shard IDENTIFYs.
    pub last_start: Option<Instant>,
    /// A copy of the sender channel to communicate with the
    /// [`ShardManagerMonitor`].
    ///
    /// [`ShardManagerMonitor`]: struct.ShardManagerMonitor.html
    pub manager_tx: UnboundedSender<ShardManagerMessage>,
    /// The shards that are queued for booting.
    ///
    /// This will typically be filled with previously failed boots.
    pub queue: VecDeque<(u64, u64)>,
    /// A receiver channel for the shard queuer to be told to start shards.
    pub rx: UnboundedReceiver<ShardQueuerMessage>,
    /// A copy of the client's voice manager.
    #[cfg(feature = "voice")]
    pub voice_manager: Arc<Mutex<ClientVoiceManager>>,
    /// A copy of the URI to use to connect to the gateway.
    pub ws_url: Arc<Mutex<String>>,
    pub cache_and_http: Arc<CacheAndHttp>,
    pub guild_subscriptions: bool,
}

impl ShardQueuer {
    /// Begins the shard queuer loop.
    ///
    /// This will loop over the internal [`rx`] for [`ShardQueuerMessage`]s,
    /// blocking for messages on what to do.
    ///
    /// If a [`ShardQueuerMessage::Start`] is received, this will:
    ///
    /// 1. Check how much time has passed since the last shard was started
    /// 2. If the amount of time is less than the ratelimit, it will sleep until
    /// that time has passed
    /// 3. Start the shard by ID
    ///
    /// If a [`ShardQueuerMessage::Shutdown`] is received, this will return and
    /// the loop will be over.
    ///
    /// **Note**: This should be run in its own thread due to the blocking
    /// nature of the loop.
    ///
    /// [`ShardQueuerMessage`]: enum.ShardQueuerMessage.html
    /// [`ShardQueuerMessage::Shutdown`]: enum.ShardQueuerMessage.html#variant.Shutdown
    /// [`ShardQueuerMessage::Start`]: enum.ShardQueuerMessage.html#variant.Start
    /// [`rx`]: #structfield.rx
    pub async fn run(&mut self) {
        while let Some(value) = self.rx.next().await {
            match value {
                ShardQueuerMessage::Shutdown => break,
                ShardQueuerMessage::Start(id, total) => {
                    self.checked_start(id.0, total.0).await;
                }
            }
        }
        /*let wait_duration = Duration::from_secs(WAIT_BETWEEN_BOOTS_IN_SECONDS);

        loop {
            match self.rx.recv_timeout(wait_duration) {
                Ok(ShardQueuerMessage::Shutdown) => break,
                Ok(ShardQueuerMessage::Start(id, total)) => {
                    self.checked_start(id.0, total.0);
                },
                Err(RecvTimeoutError::Disconnected) => {
                    // If the sender half has disconnected then the queuer's
                    // lifespan has passed and can shutdown.
                    break;
                },
                Err(RecvTimeoutError::Timeout) => {
                    if let Some((id, total)) = self.queue.pop_front() {
                        self.checked_start(id, total).await;
                    }
                }
            }
        }*/
    }

    async fn check_last_start(&mut self) {
        let instant = match self.last_start {
            Some(instant) => instant,
            None => return,
        };

        // We must wait 5 seconds between IDENTIFYs to avoid session
        // invalidations.
        let duration = Duration::from_secs(WAIT_BETWEEN_BOOTS_IN_SECONDS);
        let elapsed = instant.elapsed();

        if elapsed >= duration {
            return;
        }

        let to_sleep = duration - elapsed;

        delay_for(to_sleep).await;
    }

    async fn checked_start(&mut self, id: u64, total: u64) {
        self.check_last_start().await;

        if let Err(why) = self.start(id, total).await {
            warn!("Err starting shard {}: {:?}", id, why);
            info!("Re-queueing start of shard {}", id);

            self.queue.push_back((id, total));
        }

        self.last_start = Some(Instant::now());
    }

    async fn start(&mut self, shard_id: u64, shard_total: u64) -> Result<()> {
        let shard_info = [shard_id, shard_total];

        let shard = Shard::new(
            Arc::clone(&self.ws_url),
            &self.cache_and_http.http.token,
            shard_info,
            self.guild_subscriptions,
        ).await?;

        let mut runner = ShardRunner::new(ShardRunnerOptions {
            data: Arc::clone(&self.data),
            event_handler: self.event_handler.as_ref().map(|eh| Arc::clone(eh)),
            raw_event_handler: self.raw_event_handler.as_ref().map(|rh| Arc::clone(rh)),
            #[cfg(feature = "framework")]
            framework: Arc::clone(&self.framework),
            manager_tx: self.manager_tx.clone(),
            #[cfg(feature = "voice")]
            voice_manager: Arc::clone(&self.voice_manager),
            shard,
            cache_and_http: Arc::clone(&self.cache_and_http),
        });

        let runner_info = ShardRunnerInfo {
            latency: None,
            runner_tx: runner.runner_tx(),
            stage: ConnectionStage::Disconnected,
        };

        tokio::spawn(async move {
            let _ = runner.run().await;
        });

        self.manager_tx.unbounded_send(ShardManagerMessage::Start(ShardId(shard_id), runner_info)).unwrap();

        Ok(())
    }
}

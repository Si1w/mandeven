//! In-process message bus connecting channels and the agent.
//!
//! The bus is a pair of bounded `tokio::sync::mpsc` channels: one
//! inbound (channels → agent) and one outbound (agent → channels).
//! Streaming fragments flow as variants of [`OutboundPayload`].
//!
//! Directional access is enforced by parameterizing the generic
//! [`Sender`] and [`Receiver`] over the message type. Type aliases
//! ([`InboundSender`], [`OutboundSender`], [`InboundReceiver`],
//! [`OutboundReceiver`]) give the four concrete instantiations
//! readable names; the `T` parameter prevents cross-direction misuse
//! at compile time.
//!
//! Naming follows `tokio::sync::mpsc`: senders are cheap-to-clone,
//! receivers are single-owner, and methods are `send` / `recv`.

pub mod error;
pub mod types;

pub use error::{Error, Result};
pub use types::{
    ChannelID, InboundMessage, InboundPayload, MessageID, OutboundMessage, OutboundPayload,
    SessionID,
};

use tokio::sync::mpsc;

/// Capacity of both directional queues. A slow consumer applies
/// back-pressure on producers; messages are never silently dropped.
const BUS_CAPACITY: usize = 20;

/// Cheap-to-clone sender for bus messages of type `T`.
///
/// Directional variants are exposed via the [`InboundSender`] and
/// [`OutboundSender`] type aliases; the `T` parameter keeps them
/// non-interchangeable.
#[derive(Clone)]
pub struct Sender<T> {
    tx: mpsc::Sender<T>,
}

/// Single-owner receiver for bus messages of type `T`.
///
/// Directional variants are exposed via the [`InboundReceiver`] and
/// [`OutboundReceiver`] type aliases.
pub struct Receiver<T> {
    rx: mpsc::Receiver<T>,
}

impl<T> Sender<T> {
    /// Send one message.
    ///
    /// Awaits a queue slot when the queue is full.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Closed`] when the matching receiver has been
    /// dropped.
    pub async fn send(&self, msg: T) -> Result<()> {
        self.tx.send(msg).await.map_err(|_| Error::Closed)
    }
}

impl<T> Receiver<T> {
    /// Receive the next message.
    ///
    /// Returns `None` when the bus is closed (all senders of this
    /// direction have been dropped), signalling end-of-stream.
    pub async fn recv(&mut self) -> Option<T> {
        self.rx.recv().await
    }
}

/// Sender for channel → agent messages.
pub type InboundSender = Sender<InboundMessage>;

/// Sender for agent → channel messages.
pub type OutboundSender = Sender<OutboundMessage>;

/// Receiver for channel → agent messages.
pub type InboundReceiver = Receiver<InboundMessage>;

/// Receiver for agent → channel messages.
pub type OutboundReceiver = Receiver<OutboundMessage>;

/// Central message bus.
///
/// Holds the sender halves of the two directional queues. Receivers
/// are moved out once via [`Bus::new`]; sender halves are handed out
/// per direction via [`Bus::inbound_sender`] and
/// [`Bus::outbound_sender`].
pub struct Bus {
    inbound_tx: mpsc::Sender<InboundMessage>,
    outbound_tx: mpsc::Sender<OutboundMessage>,
}

impl Bus {
    /// Create a new bus and return the matching receiver halves.
    ///
    /// Call this exactly once per daemon lifetime. The inbound
    /// receiver goes to the agent loop; the outbound receiver goes
    /// to the channel dispatcher.
    #[must_use]
    pub fn new() -> (Self, InboundReceiver, OutboundReceiver) {
        let (inbound_tx, inbound_rx) = mpsc::channel(BUS_CAPACITY);
        let (outbound_tx, outbound_rx) = mpsc::channel(BUS_CAPACITY);
        (
            Self {
                inbound_tx,
                outbound_tx,
            },
            Receiver { rx: inbound_rx },
            Receiver { rx: outbound_rx },
        )
    }

    /// Return a sender authorized to publish inbound messages.
    #[must_use]
    pub fn inbound_sender(&self) -> InboundSender {
        Sender {
            tx: self.inbound_tx.clone(),
        }
    }

    /// Return a sender authorized to publish outbound messages.
    #[must_use]
    pub fn outbound_sender(&self) -> OutboundSender {
        Sender {
            tx: self.outbound_tx.clone(),
        }
    }
}

//! Task Orchestration System for FrankOS
//! 
//! Phase 1: Message Infrastructure
//! - LISTEN/NOTIFY PostgreSQL channels
//! - Agent message channels with priority handling
//! - Message routing and dispatch
//! - Channel lifecycle management

pub mod messages;
pub mod channels;
pub mod message_handler;
pub mod priority_queue;

pub use messages::{Message, MessageType, MessagePriority, MessagePayload};
pub use channels::{AgentChannel, ChannelManager};
pub use message_handler::{MessageHandler, HandlerResult};
pub use priority_queue::PriorityQueue;

/// Phase 1: Message Infrastructure
/// Provides the foundation for all inter-agent and system communication
/// using PostgreSQL LISTEN/NOTIFY with in-memory channel management
pub struct MessageInfrastructure {
    channel_manager: ChannelManager,
    handler: MessageHandler,
}

impl MessageInfrastructure {
    pub fn new(pool: sqlx::PgPool) -> Self {
        Self {
            channel_manager: ChannelManager::new(pool.clone()),
            handler: MessageHandler::new(pool),
        }
    }

    pub fn channel_manager(&self) -> &ChannelManager {
        &self.channel_manager
    }

    pub fn handler(&self) -> &MessageHandler {
        &self.handler
    }
}

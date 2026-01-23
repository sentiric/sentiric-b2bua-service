// sentiric-b2bua-service/src/rabbitmq.rs

use anyhow::Result;
use lapin::{options::*, BasicProperties, Channel, Connection, ConnectionProperties};
use serde::Serialize;
use tracing::{info, debug};
use std::sync::Arc;

pub struct RabbitMqClient {
    channel: Arc<Channel>,
}

impl RabbitMqClient {
    pub async fn new(url: &str) -> Result<Self> {
        let conn = Connection::connect(url, ConnectionProperties::default()).await?;
        info!("RabbitMQ bağlantısı sağlandı.");
        let channel = conn.create_channel().await?;
        Ok(Self {
            channel: Arc::new(channel),
        })
    }

    pub async fn publish_event<T: Serialize>(&self, routing_key: &str, event: &T) -> Result<()> {
        let payload = serde_json::to_vec(event)?;
        
        self.channel
            .basic_publish(
                "sentiric_events", // Exchange name (Infrastructure tarafında tanımlı)
                routing_key,
                BasicPublishOptions::default(),
                &payload,
                BasicProperties::default().with_delivery_mode(2), // Persistent
            )
            .await?;
            
        debug!("Event yayınlandı: {}", routing_key);
        Ok(())
    }
}
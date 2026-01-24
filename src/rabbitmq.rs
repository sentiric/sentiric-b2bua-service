// sentiric-b2bua-service/src/rabbitmq.rs

use anyhow::Result;
use lapin::{options::*, BasicProperties, Channel, Connection, ConnectionProperties};
// serde importu kaldırıldı çünkü binary için gerek yok
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

    // YENİ METOT: Binary Veri Yayınlama
    pub async fn publish_event_bytes(&self, routing_key: &str, payload: &[u8]) -> Result<()> {
        self.channel
            .basic_publish(
                "sentiric_events", 
                routing_key,
                BasicPublishOptions::default(),
                payload, // Direkt binary data
                BasicProperties::default().with_delivery_mode(2), // Persistent
            )
            .await?;
            
        debug!("Binary Event yayınlandı: {} ({} bytes)", routing_key, payload.len());
        Ok(())
    }
}
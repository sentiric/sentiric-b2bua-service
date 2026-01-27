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

    // ✅ GÜNCELLEME: Retry Mekanizmalı Binary Veri Yayınlama
    pub async fn publish_event_bytes(&self, routing_key: &str, payload: &[u8]) -> Result<()> {
        const MAX_RETRIES: u32 = 3;
        
        for attempt in 0..MAX_RETRIES {
            match self.channel
                .basic_publish(
                    "sentiric_events", 
                    routing_key,
                    BasicPublishOptions::default(),
                    payload,
                    BasicProperties::default().with_delivery_mode(2), // Persistent
                )
                .await
            {
                Ok(_) => {
                    if attempt > 0 {
                        info!("✅ [MQ] Event published after {} retries: {}", attempt, routing_key);
                    } else {
                        debug!("📨 [MQ] Event published: {} ({} bytes)", routing_key, payload.len());
                    }
                    return Ok(());
                }
                Err(e) => {
                    tracing::error!(
                        "❌ [MQ] Publish failed (attempt {}/{}): {}",
                        attempt + 1,
                        MAX_RETRIES,
                        e
                    );
                    
                    if attempt < MAX_RETRIES - 1 {
                        tokio::time::sleep(tokio::time::Duration::from_millis(100 * (attempt + 1) as u64)).await;
                    } else {
                        return Err(anyhow::anyhow!("RabbitMQ publish failed after {} retries", MAX_RETRIES));
                    }
                }
            }
        }
        
        unreachable!()
    }
}
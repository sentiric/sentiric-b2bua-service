// sentiric-b2bua-service/src/rabbitmq.rs

use anyhow::Result;
use lapin::{options::*, types::FieldTable, BasicProperties, Channel, Connection, ConnectionProperties};
use futures::StreamExt; // [DÜZELTME]: futures kütüphanesinden import edildi
use tracing::{info, debug};
use std::sync::Arc;

pub struct RabbitMqClient {
    channel: Arc<Channel>,
}

impl RabbitMqClient {
    pub async fn new(url: &str) -> Result<Self> {
        let mut attempt = 0;
        let conn = loop {
            attempt += 1;
            match Connection::connect(url, ConnectionProperties::default()).await {
                Ok(c) => break c,
                Err(e) => {
                    if attempt >= 10 { anyhow::bail!("RabbitMQ'ya bağlanılamadı: {}", e); }
                    tracing::warn!("RabbitMQ bağlantısı bekleniyor... ({}/10)", attempt);
                    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
                }
            }
        };
        info!("✅ [MQ] RabbitMQ bağlantısı sağlandı.");
        let channel = conn.create_channel().await?;
        Ok(Self {
            channel: Arc::new(channel),
        })
    }

    pub async fn publish_event_bytes(&self, routing_key: &str, payload: &[u8]) -> Result<()> {
        const MAX_RETRIES: u32 = 3;
        
        for attempt in 0..MAX_RETRIES {
            match self.channel
                .basic_publish(
                    "sentiric_events", 
                    routing_key,
                    BasicPublishOptions::default(),
                    payload,
                    BasicProperties::default().with_delivery_mode(2),
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
                    tracing::error!("❌ [MQ] Publish failed (attempt {}/{}): {}", attempt + 1, MAX_RETRIES, e);
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

    pub async fn start_termination_consumer(&self, engine: std::sync::Arc<crate::sip::engine::B2BuaEngine>) {
        let channel = self.channel.clone();
        tokio::spawn(async move {
            let queue_name = "sentiric.b2bua_service.commands";
            let _ = channel.queue_declare(queue_name, QueueDeclareOptions { durable: true, ..Default::default() }, FieldTable::default()).await;
            let _ = channel.queue_bind(queue_name, "sentiric_events", "call.terminate.request", QueueBindOptions::default(), FieldTable::default()).await;
            
            if let Ok(mut consumer) = channel.basic_consume(queue_name, "b2bua_term_consumer", BasicConsumeOptions::default(), FieldTable::default()).await {
                tracing::info!("👂 [MQ] B2BUA Termination Consumer (Sonlandırıcı) dinlemeye başladı.");
                while let Some(delivery) = consumer.next().await {
                    if let Ok(delivery) = delivery {
                        if let Ok(json) = serde_json::from_slice::<serde_json::Value>(&delivery.data) {
                            if let Some(call_id) = json["callId"].as_str() {
                                tracing::info!("🛑 [MQ] Termination isteği alındı. Çağrı kesiliyor: {}", call_id);
                                engine.terminate_session(call_id).await;
                            }
                        }
                        let _ = delivery.ack(BasicAckOptions::default()).await;
                    }
                }
            }
        });
    }
}
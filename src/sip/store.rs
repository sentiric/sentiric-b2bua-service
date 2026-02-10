// sentiric-b2bua-service/src/sip/store.rs

use std::sync::Arc;
use dashmap::DashMap;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;
use tracing::{error, info, debug};
use sentiric_sip_core::transaction::SipTransaction;
use sentiric_rtp_core::RtpEndpoint;

// --- Veri Modelleri ---

/// Redis'e kaydedilecek saf veri modeli (Serializable)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum CallState {
    Null,
    Trying,
    Ringing,
    Established,
    Terminated,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallSessionData {
    pub call_id: String,
    pub state: CallState,
    pub from_uri: String,
    pub to_uri: String,
    pub rtp_port: u32,
    pub local_tag: String,
}

/// Uygulama içinde kullanılacak zengin model (Runtime Objects içerir)
#[derive(Debug, Clone)]
pub struct CallSession {
    pub data: CallSessionData,
    // Aşağıdakiler Redis'e yazılmaz, Runtime'da yönetilir
    pub endpoint: RtpEndpoint,
    pub active_transaction: Option<SipTransaction>,
}

impl CallSession {
    pub fn new(data: CallSessionData) -> Self {
        Self {
            data,
            endpoint: RtpEndpoint::new(None),
            active_transaction: None,
        }
    }
}

// --- Store Implementation ---

#[derive(Clone)]
pub struct CallStore {
    // L1 Cache: Hızlı erişim için RAM
    local_cache: Arc<DashMap<String, CallSession>>,
    // L2 Cache: Kalıcılık için Redis
    redis: Arc<Mutex<redis::aio::MultiplexedConnection>>,
}

impl CallStore {
    pub async fn new(redis_url: &str) -> anyhow::Result<Self> {
        let client = redis::Client::open(redis_url)?;
        let conn = client.get_multiplexed_async_connection().await?;
        
        Ok(Self {
            local_cache: Arc::new(DashMap::new()),
            redis: Arc::new(Mutex::new(conn)),
        })
    }

    /// Yeni bir çağrı oturumu başlatır ve her iki katmana da yazar.
    pub async fn insert(&self, session: CallSession) {
        let call_id = session.data.call_id.clone();
        
        // 1. RAM'e yaz
        self.local_cache.insert(call_id.clone(), session.clone());

        // 2. Redis'e yaz (Fire & Forget tarzı, hata olursa logla ama akışı kesme)
        if let Ok(json) = serde_json::to_string(&session.data) {
            let mut conn = self.redis.lock().await;
            let key = format!("b2bua:call:{}", call_id);
            // 24 saat TTL (Call leak olursa temizlensin diye)
            if let Err(e) = conn.set_ex::<_, _, ()>(&key, json, 86400).await {
                error!("Redis write error for {}: {}", call_id, e);
            } else {
                debug!("💾 Call session persisted to Redis: {}", call_id);
            }
        }
    }

    /// Çağrı durumunu günceller.
    pub async fn update_state(&self, call_id: &str, new_state: CallState) {
        if let Some(mut entry) = self.local_cache.get_mut(call_id) {
            entry.data.state = new_state.clone();
            
            // Redis güncellemesi
            if let Ok(json) = serde_json::to_string(&entry.data) {
                let mut conn = self.redis.lock().await;
                let key = format!("b2bua:call:{}", call_id);
                // Mevcut TTL'i korumak için set_xx kullanılabilir ama basitçe set_ex yapıyoruz
                let _: () = conn.set_ex(&key, json, 86400).await.unwrap_or_default();
            }
        }
    }

    /// Çağrıyı okur (Önce RAM, yoksa Redis).
    /// Redis'ten gelirse Runtime objelerini (Endpoint vb.) sıfırdan oluşturur.
    pub async fn get(&self, call_id: &str) -> Option<CallSession> {
        // 1. RAM Kontrolü
        if let Some(session) = self.local_cache.get(call_id) {
            return Some(session.clone());
        }

        // 2. Redis Kontrolü (Failover Senaryosu)
        let key = format!("b2bua:call:{}", call_id);
        let mut conn = self.redis.lock().await;
        
        match conn.get::<_, String>(&key).await {
            Ok(json) => {
                if let Ok(data) = serde_json::from_str::<CallSessionData>(&json) {
                    info!("♻️ Session restored from Redis (Hydration): {}", call_id);
                    let session = CallSession::new(data);
                    // Tekrar RAM'e al
                    self.local_cache.insert(call_id.to_string(), session.clone());
                    return Some(session);
                }
            },
            Err(_) => {} // Bulunamadı
        }

        None
    }

    /// Çağrıyı siler (BYE).
    pub async fn remove(&self, call_id: &str) -> Option<CallSession> {
        let key = format!("b2bua:call:{}", call_id);
        let mut conn = self.redis.lock().await;
        let _: () = conn.del(&key).await.unwrap_or_default();
        
        self.local_cache.remove(call_id).map(|(_, s)| s)
    }
}
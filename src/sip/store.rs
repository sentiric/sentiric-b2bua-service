// src/sip/store.rs

use std::sync::Arc;
use dashmap::DashMap;
use redis::AsyncCommands;
use redis::aio::ConnectionManager;
use serde::{Deserialize, Serialize};
use tracing::{error, info, debug};
use sentiric_sip_core::transaction::SipTransaction;
use sentiric_rtp_core::RtpEndpoint;

// --- Veri Modelleri ---

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

#[derive(Debug, Clone)]
pub struct CallSession {
    pub data: CallSessionData,
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
    local_cache: Arc<DashMap<String, CallSession>>,
    redis: ConnectionManager,
}

impl CallStore {
    pub async fn new(redis_url: &str) -> anyhow::Result<Self> {
        let client = redis::Client::open(redis_url)?;
        let conn = ConnectionManager::new(client).await?;
        
        Ok(Self {
            local_cache: Arc::new(DashMap::new()),
            redis: conn,
        })
    }

    pub async fn insert(&self, session: CallSession) {
        let call_id = session.data.call_id.clone();
        
        self.local_cache.insert(call_id.clone(), session.clone());

        if let Ok(json) = serde_json::to_string(&session.data) {
            let mut conn = self.redis.clone();
            let key = format!("b2bua:call:{}", call_id);
            
            // [DÜZELTME]: Explicit Type Binding
            let result: redis::RedisResult<()> = conn.set_ex(&key, json, 86400).await;
            if let Err(e) = result {
                error!("Redis write error for {}: {}", call_id, e);
            } else {
                debug!("💾 Call session persisted to Redis: {}", call_id);
            }
        }
    }

    pub async fn update_state(&self, call_id: &str, new_state: CallState) {
        if let Some(mut entry) = self.local_cache.get_mut(call_id) {
            entry.data.state = new_state.clone();
            
            if let Ok(json) = serde_json::to_string(&entry.data) {
                let mut conn = self.redis.clone();
                let key = format!("b2bua:call:{}", call_id);
                // [DÜZELTME]: Explicit Type Binding
                let _: redis::RedisResult<()> = conn.set_ex(&key, json, 86400).await;
            }
        }
    }

    pub async fn get(&self, call_id: &str) -> Option<CallSession> {
        if let Some(session) = self.local_cache.get(call_id) {
            return Some(session.clone());
        }

        let key = format!("b2bua:call:{}", call_id);
        let mut conn = self.redis.clone();
        
        // [DÜZELTME]: Explicit Type Binding (E0277 Hatasını Çözer)
        let result: redis::RedisResult<String> = conn.get(&key).await;
        match result {
            Ok(json) => {
                if let Ok(data) = serde_json::from_str::<CallSessionData>(&json) {
                    info!("♻️ Session restored from Redis (Hydration): {}", call_id);
                    let session = CallSession::new(data);
                    self.local_cache.insert(call_id.to_string(), session.clone());
                    return Some(session);
                }
            },
            Err(_) => {} 
        }
        None
    }

    pub async fn remove(&self, call_id: &str) -> Option<CallSession> {
        let key = format!("b2bua:call:{}", call_id);
        let mut conn = self.redis.clone();
        
        // [DÜZELTME]: Explicit Type Binding
        let _: redis::RedisResult<()> = conn.del(&key).await;
        
        self.local_cache.remove(call_id).map(|(_, s)| s)
    }
}
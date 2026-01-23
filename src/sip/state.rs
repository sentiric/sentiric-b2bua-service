// sentiric-b2bua-service/src/sip/state.rs

use std::sync::Arc;
use dashmap::DashMap;
use std::net::SocketAddr; // EKLENDİ

#[derive(Debug, Clone)]
pub struct CallSession {
    pub call_id: String,
    pub state: CallState,
    pub from_uri: String,
    pub to_uri: String,
    pub rtp_port: u32,
    pub remote_tag: Option<String>,
    // YENİ ALANLAR: Bridging için
    pub peer_call_id: Option<String>,
    pub source_addr: Option<SocketAddr>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum CallState {
    Trying,
    Ringing,
    Established,
    Terminated,
}

pub type CallStore = Arc<DashMap<String, CallSession>>;

pub fn new_store() -> CallStore {
    Arc::new(DashMap::new())
}
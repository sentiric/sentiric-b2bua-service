// sentiric-b2bua-service/src/sip/state.rs
use std::sync::Arc;
use std::net::SocketAddr;
use dashmap::DashMap;
use sentiric_sip_core::SipTransaction; // DÜZELTME: SipPacket kaldırıldı

#[derive(Debug, Clone, PartialEq)]
pub enum CallState {
    Null,
    Trying,
    Ringing,
    Established,
    Terminated,
}

#[derive(Debug, Clone)]
pub struct CallSession {
    pub call_id: String,
    pub state: CallState,
    
    // Kimlikler
    pub from_uri: String,
    pub to_uri: String,
    
    // Medya Bilgileri
    pub rtp_port: u32,
    
    // SIP Transaction State
    pub local_tag: String,
    pub remote_tag: Option<String>,
    
    // Routing Bilgisi
    pub caller_addr: Option<SocketAddr>,
    pub callee_addr: Option<SocketAddr>,
    
    // Bridging
    pub is_bridged: bool,
    pub peer_call_id: Option<String>,

    // Transaction State Machine Object
    pub active_transaction: Option<SipTransaction>,
}

pub type CallStore = Arc<DashMap<String, CallSession>>;

pub fn new_store() -> CallStore {
    Arc::new(DashMap::new())
}
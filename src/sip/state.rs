// sentiric-b2bua-service/src/sip/state.rs

use std::sync::Arc;
use dashmap::DashMap;
use sentiric_sip_core::SipPacket;

#[derive(Debug, Clone)]
pub struct CallSession {
    pub call_id: String,
    pub state: CallState,
    pub from_uri: String,
    pub to_uri: String,
    
    // Medya Bilgileri (Media Service'ten gelen)
    pub rtp_port: u32,
    
    // SIP Transaction State (Retransmission için kritik)
    pub local_tag: String,

    // [YENİ] Idempotency Cache: 
    // Üretilen son 200 OK yanıtını saklarız. 
    // Tekrar (Retransmission) gelirse bunu aynen döneriz.
    pub last_invite_response: Option<SipPacket>, 
}

#[derive(Debug, Clone, PartialEq)]
pub enum CallState {
    Invited,
    Established,
    Terminated,
}

pub type CallStore = Arc<DashMap<String, CallSession>>;

pub fn new_store() -> CallStore {
    Arc::new(DashMap::new())
}
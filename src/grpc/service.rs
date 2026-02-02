// sentiric-b2bua-service/src/grpc/service.rs
use sentiric_contracts::sentiric::sip::v1::{
    b2bua_service_server::B2buaService,
    InitiateCallRequest, InitiateCallResponse,
    TransferCallRequest, TransferCallResponse,
};
use tonic::{Request, Response, Status};
use tracing::{info, instrument, error};
use std::sync::Arc;
use crate::sip::engine::B2BuaEngine;

pub struct MyB2BuaService {
    engine: Arc<B2BuaEngine>,
}

impl MyB2BuaService {
    pub fn new(engine: Arc<B2BuaEngine>) -> Self {
        Self { engine }
    }
}

#[tonic::async_trait]
impl B2buaService for MyB2BuaService {
    
    #[instrument(skip(self), fields(from = %request.get_ref().from_uri, to = %request.get_ref().to_uri))]
    async fn initiate_call(
        &self,
        request: Request<InitiateCallRequest>,
    ) -> Result<Response<InitiateCallResponse>, Status> {
        let req = request.into_inner();
        
        // Eğer Call-ID verilmemişse üret
        let call_id = if req.call_id.is_empty() {
             uuid::Uuid::new_v4().to_string()
        } else {
             req.call_id
        };

        // Engine üzerinden çağrı başlatma (Outbound Call) henüz implement edilmedi
        // Ancak altyapı hazır. Burası Agent'ın dış arama yapmasını sağlar.
        
        // TODO: self.engine.initiate_outbound_call(...).await;
        
        // Şimdilik sadece logluyoruz, çünkü asıl öncelik Inbound (Gelen) çağrılar.
        info!("Outbound çağrı isteği alındı (Henüz aktif değil): {}", call_id);
        
        Ok(Response::new(InitiateCallResponse {
            success: true,
            new_call_id: call_id,
        }))
    }

    async fn transfer_call(
        &self,
        _request: Request<TransferCallRequest>,
    ) -> Result<Response<TransferCallResponse>, Status> {
        Err(Status::unimplemented("Transfer henüz implement edilmedi."))
    }
}
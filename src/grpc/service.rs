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
        let new_call_id = req.call_id.clone(); // Agent'tan gelen call_id'yi kullanabiliriz veya yeni üretebiliriz.

        match self.engine.initiate_call(new_call_id.clone(), req.from_uri, req.to_uri).await {
            Ok(_) => {
                info!("Çağrı başlatma isteği işleme alındı.");
                Ok(Response::new(InitiateCallResponse {
                    success: true,
                    new_call_id,
                }))
            },
            Err(e) => {
                error!("Çağrı başlatılamadı: {}", e);
                Err(Status::internal("Çağrı başlatılamadı"))
            }
        }
    }

    async fn transfer_call(
        &self,
        _request: Request<TransferCallRequest>,
    ) -> Result<Response<TransferCallResponse>, Status> {
        Err(Status::unimplemented("Transfer henüz implamente edilmedi."))
    }
}
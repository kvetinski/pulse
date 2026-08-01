use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use pulse_demo::demo_service_server::{DemoService, DemoServiceServer};
use pulse_demo::{EchoRequest, EchoResponse};
use tonic::{transport::Server, Request, Response, Status};

mod pulse_demo {
    tonic::include_proto!("pulse.demo.v1");
}

#[derive(Debug)]
struct DemoTarget {
    instance: String,
    request_sequence: AtomicU64,
}

#[tonic::async_trait]
impl DemoService for DemoTarget {
    async fn echo(&self, request: Request<EchoRequest>) -> Result<Response<EchoResponse>, Status> {
        let request = request.into_inner();
        let sequence = self.request_sequence.fetch_add(1, Ordering::Relaxed) + 1;

        if request.delay_ms > 0 {
            tokio::time::sleep(Duration::from_millis(u64::from(request.delay_ms))).await;
        }
        println!(
            "demo_target_request sequence={sequence} method=pulse.demo.v1.DemoService/Echo instance={} message={:?} delay_ms={} outcome={}",
            self.instance,
            request.message,
            request.delay_ms,
            if request.fail { "unavailable" } else { "ok" }
        );
        if request.fail {
            return Err(Status::unavailable("deterministic demo failure requested"));
        }

        Ok(Response::new(EchoResponse {
            message: request.message,
            request_sequence: sequence,
            target_instance: self.instance.clone(),
        }))
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).is_some_and(|arg| arg == "--healthcheck") {
        return healthcheck(args.get(2).map(String::as_str).unwrap_or("127.0.0.1:50051"));
    }

    let bind = std::env::var("PULSE_DEMO_TARGET_BIND")
        .unwrap_or_else(|_| "0.0.0.0:50051".to_string())
        .parse::<SocketAddr>()?;
    let instance = std::env::var("PULSE_DEMO_TARGET_INSTANCE")
        .unwrap_or_else(|_| "local-fixture-1".to_string());
    let target = DemoTarget {
        instance,
        request_sequence: AtomicU64::new(0),
    };

    println!("demo_target_listening address={bind}");
    Server::builder()
        .add_service(DemoServiceServer::new(target))
        .serve_with_shutdown(bind, shutdown_signal())
        .await?;
    Ok(())
}

fn healthcheck(address: &str) -> Result<(), Box<dyn std::error::Error>> {
    let address = address.parse::<SocketAddr>()?;
    TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut signal) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            signal.recv().await;
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}

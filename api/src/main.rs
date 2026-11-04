mod aeron_handler;
use aeron_handler::{OrderSender, spawn_order_publisher};

mod order;

mod routes;
use routes::{get_order, post_order};

mod errors;

use std::sync::Arc;
use std::time::Instant;

use axum::{Router, routing::get};

use log::{error, info};

pub struct AppState {
    orders: OrderSender,
}

#[tokio::main]
async fn main() {
    env_logger::init();

    let start_time = Instant::now();

    info!("Aeron: Using directory: {:?}", transport::aeron_dir());

    let channel = transport::env_or("PUB_CHANNEL", transport::ORDERS_CHANNEL);
    let stream_id = transport::stream_id_or("PUB_STREAM_ID", transport::ORDERS_STREAM);

    let orders = match spawn_order_publisher(&channel, stream_id) {
        Ok(orders) => orders,
        Err(e) => {
            error!("Aeron: Failed to create publication: {}", e);
            return;
        }
    };

    let shared_state = Arc::new(AppState { orders });

    let app = Router::new().route(
        "/api/v1/orders",
        get(get_order).post(post_order).with_state(shared_state),
    );

    let listener = match tokio::net::TcpListener::bind("0.0.0.0:8000").await {
        Ok(listener) => {
            info!("Server: Bound to 0.0.0.0:8000");
            listener
        }
        Err(e) => {
            error!("Server: Failed to bind to port 8000: {}", e);
            return;
        }
    };

    let startup_duration = start_time.elapsed();
    info!(
        "Startup complete in {:.2?} — ready to accept requests",
        startup_duration
    );

    if let Err(e) = axum::serve(listener, app).await {
        error!("Server failed: {}", e);
    }
}

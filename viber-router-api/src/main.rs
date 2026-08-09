mod cache;
mod circuit_breaker;
mod config;
mod db;
mod log_buffer;
mod middleware;
mod models;
mod partition;
mod rate_limiter;
mod redis;
mod routes;
mod serde_utils;
mod sse_usage_parser;
mod subscription;
mod translate;
mod telegram_notifier;
mod ttft_buffer;
mod uptime_buffer;
mod usage_buffer;

use std::time::Duration;

use anyhow::Result;
use tracing_subscriber::EnvFilter;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use tokio::sync::RwLock;

#[tokio::main]
async fn main() -> Result<()> {
    let config = config::Config::load()?;

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.rust_log)),
        )
        .init();

    tracing::info!("Starting viber-router-api");

    let db_pool = db::create_pool(&config).await?;
    tracing::info!("Connected to PostgreSQL");

    sqlx::migrate!("./migrations").run(&db_pool).await?;
    tracing::info!("Migrations applied");

    // Ensure partitions exist for current and next month
    partition::ensure_partitions(&db_pool, "proxy_logs").await;
    partition::ensure_partitions(&db_pool, "ttft_logs").await;
    partition::ensure_partitions(&db_pool, "token_usage_logs").await;
    partition::ensure_partitions(&db_pool, "uptime_checks").await;
    tracing::info!("Partitions ensured");

    let redis_pool = redis::create_pool(&config)?;
    tracing::info!("Redis pool created");

    // Create log buffer
    let (log_tx, log_rx) = tokio::sync::mpsc::channel(10_000);

    // Create TTFT log buffer
    let (ttft_tx, ttft_rx) = tokio::sync::mpsc::channel(10_000);

    // Create token usage log buffer
    let (usage_tx, usage_rx) = tokio::sync::mpsc::channel(10_000);

    // Create uptime check buffer
    let (uptime_tx, uptime_rx) = tokio::sync::mpsc::channel(10_000);

    // Create and populate pricing cache
    let pricing_cache: routes::PricingCache = Arc::new(RwLock::new(HashMap::new()));
    routes::refresh_pricing_cache(&db_pool, &pricing_cache).await;
    tracing::info!("Pricing cache loaded");

    // Server unlock session state — in-memory, per-process
    let unlocked_servers: routes::UnlockedServers = Arc::new(RwLock::new(HashSet::new()));

    let state = routes::AppState {
        db: db_pool.clone(),
        redis: redis_pool,
        admin_token: config.admin_token,
        http_client: reqwest::Client::builder()
            .timeout(Duration::from_secs(8 * 3600))
            .connect_timeout(Duration::from_secs(10))
            .pool_idle_timeout(Duration::from_secs(3600))
            .tcp_keepalive(Duration::from_secs(30))
            .tcp_nodelay(true)
            .http2_keep_alive_interval(Duration::from_secs(30))
            .http2_keep_alive_timeout(Duration::from_secs(10))
            .http2_keep_alive_while_idle(true)
            .build()
            .expect("Failed to build HTTP client"),
        log_tx,
        ttft_tx,
        usage_tx,
        uptime_tx,
        pricing_cache: pricing_cache.clone(),
        unlocked_servers: unlocked_servers.clone(),
    };

    // Spawn log buffer flush task
    let flush_handle = tokio::spawn(log_buffer::flush_task(log_rx, db_pool.clone()));

    // Spawn TTFT buffer flush task
    let ttft_flush_handle = tokio::spawn(ttft_buffer::flush_task(ttft_rx, db_pool.clone()));

    // Spawn token usage buffer flush task
    let usage_flush_handle = tokio::spawn(usage_buffer::flush_task(usage_rx, db_pool.clone()));

    // Spawn uptime check buffer flush task
    let uptime_flush_handle = tokio::spawn(uptime_buffer::flush_task(uptime_rx, db_pool.clone()));

    // Spawn daily partition maintenance
    let retention_days = config.log_retention_days;
    let partition_pool = db_pool.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(86400));
        interval.tick().await; // skip immediate tick
        loop {
            interval.tick().await;
            partition::ensure_partitions(&partition_pool, "proxy_logs").await;
            partition::ensure_partitions(&partition_pool, "ttft_logs").await;
            partition::ensure_partitions(&partition_pool, "token_usage_logs").await;
            partition::ensure_partitions(&partition_pool, "uptime_checks").await;
            partition::drop_expired_partitions(&partition_pool, "proxy_logs", retention_days).await;
            partition::drop_expired_partitions(&partition_pool, "ttft_logs", retention_days).await;
            partition::drop_expired_partitions(&partition_pool, "token_usage_logs", retention_days)
                .await;
            partition::drop_expired_partitions(&partition_pool, "uptime_checks", retention_days)
                .await;
            tracing::info!("Daily partition maintenance complete");
        }
    });

    // Spawn the daily midnight proxy-log purge. Fires at 00:00 in the configured
    // timezone setting and removes proxy logs older than `proxy_log_retention_days`
    // (default 3). The purge drops whole expired monthly partitions and DELETE+VACUUMs
    // the overlapping one, so the DB size stays bounded; log writes go through the mpsc
    // buffer, so request relay is never affected.
    let purge_pool = db_pool.clone();
    tokio::spawn(async move {
        use chrono::{Duration as ChronoDuration, TimeZone, Utc};
        loop {
            // Read timezone + retention fresh each cycle so admin UI changes take effect.
            let (tz_str, keep_days) = sqlx::query_as::<_, (String, i32)>(
                "SELECT timezone, proxy_log_retention_days FROM settings WHERE id = 1",
            )
            .fetch_optional(&purge_pool)
            .await
            .ok()
            .flatten()
            .unwrap_or_else(|| (cache::DEFAULT_TIMEZONE.to_string(), 3));

            let tz: chrono_tz::Tz = tz_str
                .parse()
                .unwrap_or_else(|_| cache::DEFAULT_TIMEZONE.parse().unwrap());

            // Compute the next local midnight in `tz`, then the UTC instant to sleep until.
            let now_local = Utc::now().with_timezone(&tz);
            let today = now_local.date_naive();
            let mut next_midnight_date = today;
            // If it's already past midnight today (it always is unless exactly 00:00:00),
            // target tomorrow.
            if now_local.time() != chrono::NaiveTime::MIN {
                next_midnight_date = today + ChronoDuration::days(1);
            }
            let next_midnight_naive = next_midnight_date.and_time(chrono::NaiveTime::MIN);
            // Resolve the local naive midnight to a concrete UTC instant (handles DST).
            let next_midnight_utc = match tz.from_local_datetime(&next_midnight_naive) {
                chrono::LocalResult::Single(dt) => dt.with_timezone(&Utc),
                chrono::LocalResult::Ambiguous(dt, _) => dt.with_timezone(&Utc),
                chrono::LocalResult::None => {
                    // Midnight skipped by DST spring-forward — target one hour later,
                    // which is guaranteed to exist.
                    match tz.from_local_datetime(&(next_midnight_naive + ChronoDuration::hours(1))) {
                        chrono::LocalResult::Single(dt)
                        | chrono::LocalResult::Ambiguous(dt, _) => dt.with_timezone(&Utc),
                        chrono::LocalResult::None => next_midnight_naive.and_utc(),
                    }
                }
            };

            let sleep_secs = (next_midnight_utc - Utc::now()).num_seconds().max(1) as u64;
            tokio::time::sleep(Duration::from_secs(sleep_secs)).await;

            match routes::admin::logs::purge_proxy_logs(&purge_pool, i64::from(keep_days)).await {
                Ok(deleted) => tracing::info!(
                    "Daily midnight purge ({tz_str}): removed {deleted} proxy logs older than {keep_days} days"
                ),
                Err(e) => tracing::warn!("Daily midnight purge failed: {e}"),
            }
        }
    });

    // Spawn pricing cache refresh task (every 60 seconds)
    let pricing_pool = db_pool.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_secs(60));
        interval.tick().await; // skip immediate tick
        loop {
            interval.tick().await;
            routes::refresh_pricing_cache(&pricing_pool, &pricing_cache).await;
        }
    });

    let app = routes::router(state.clone());
    let addr = format!("{}:{}", config.host, config.port);
    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!("Listening on {}", addr);

    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;

    // Drop state to close the log channel sender, then drain the flush tasks
    drop(state);
    let _ = flush_handle.await;
    let _ = ttft_flush_handle.await;
    let _ = usage_flush_handle.await;
    let _ = uptime_flush_handle.await;

    tracing::info!("Server shut down");
    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("Failed to install ctrl+c handler");
    tracing::info!("Shutdown signal received");
}

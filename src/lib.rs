pub mod admin;
pub mod backends;
pub mod config;
pub mod discovery;
pub mod sampler;
pub mod shard;
pub mod stats;
pub mod statsd_client;
pub mod statsd_server;
pub mod statsd_proto;
pub mod built_info {
    // The file has been placed there by the build script.
    include!(concat!(env!("OUT_DIR"), "/built.rs"));
}

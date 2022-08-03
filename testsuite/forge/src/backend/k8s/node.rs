// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::{
    get_free_port, scale_stateful_set_replicas, FullNode, HealthCheckError, Node, NodeExt, Result,
    Validator, Version, KUBECTL_BIN,
};
use anyhow::{anyhow, format_err, Context};
use aptos_config::config::NodeConfig;
use aptos_logger::info;
use aptos_rest_client::Client as RestClient;
use aptos_sdk::types::PeerId;
use reqwest::Url;
use serde_json::Value;
use std::{
    fmt::{Debug, Formatter},
    process::{Command, Stdio},
    str::FromStr,
    thread,
    time::{Duration, Instant},
};

const NODE_METRIC_PORT: u64 = 9101;

// this is the port on the validator service itself, as opposed to 80 on the validator haproxy service
pub const REST_API_SERVICE_PORT: u32 = 8080;
pub const REST_API_HAPROXY_SERVICE_PORT: u32 = 80;

// when we interact with the node over port-forward
const LOCALHOST: &str = "127.0.0.1";

pub struct K8sNode {
    pub(crate) name: String,
    pub(crate) stateful_set_name: String,
    pub(crate) peer_id: PeerId,
    pub(crate) index: usize,
    pub(crate) service_name: String,
    pub(crate) rest_api_port: u32,
    pub version: Version,
    pub namespace: String,
    // whether this node has HAProxy in front of it
    pub haproxy_enabled: bool,
    // whether we should try using port-forward on the Service to reach this node
    pub port_forward_enabled: bool,
}

impl K8sNode {
    fn rest_api_port(&self) -> u32 {
        self.rest_api_port
    }

    fn service_name(&self) -> String {
        self.service_name.clone()
    }

    #[allow(dead_code)]
    fn index(&self) -> usize {
        self.index
    }

    pub(crate) fn rest_client(&self) -> RestClient {
        RestClient::new(self.rest_api_endpoint())
    }

    pub fn stateful_set_name(&self) -> &str {
        &self.stateful_set_name
    }

    fn namespace(&self) -> &str {
        &self.namespace
    }

    pub fn spawn_port_forward(&self) -> Result<()> {
        let remote_rest_api_port = if self.haproxy_enabled {
            REST_API_HAPROXY_SERVICE_PORT
        } else {
            REST_API_SERVICE_PORT
        };
        let port_forward_args = [
            "port-forward",
            "-n",
            self.namespace(),
            &format!("svc/{}", self.service_name()),
            &format!("{}:{}", self.rest_api_port(), remote_rest_api_port),
        ];
        // spawn a port-forward child process
        let cmd = Command::new(KUBECTL_BIN)
            .args(port_forward_args)
            .stdout(Stdio::null())
            // .stderr(Stdio::null())
            .spawn();
        match cmd {
            Ok(mut child) => {
                // sleep a bit and check if port-forward failed for some reason
                let timeout = Duration::from_secs(1);
                thread::sleep(timeout);
                match child.try_wait() {
                    Ok(Some(status)) => {
                        info!("Port-forward may have started already: exit {}", status);
                        Ok(())
                    }
                    Ok(None) => {
                        info!("Port-forward started for {:?}", self);
                        Ok(())
                    }
                    Err(err) => Err(anyhow!(
                        "Port-forward did not work: {:?} error {}",
                        port_forward_args,
                        err
                    )),
                }
            }
            Err(err) => Err(anyhow!(
                "Port-forward did not start: {:?} error {}",
                port_forward_args,
                err
            )),
        }
    }
}

#[async_trait::async_trait]
impl Node for K8sNode {
    fn peer_id(&self) -> PeerId {
        self.peer_id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn version(&self) -> Version {
        self.version.clone()
    }

    fn rest_api_endpoint(&self) -> Url {
        let host = if self.port_forward_enabled {
            LOCALHOST
        } else {
            &self.service_name
        };
        Url::from_str(&format!("http://{}:{}", host, self.rest_api_port())).expect("Invalid URL.")
    }

    // TODO: verify this still works
    fn inspection_service_endpoint(&self) -> Url {
        Url::parse(&format!(
            "http://{}:{}",
            &self.service_name(),
            self.rest_api_port()
        ))
        .unwrap()
    }

    fn config(&self) -> &NodeConfig {
        todo!()
    }

    async fn start(&mut self) -> Result<()> {
        scale_stateful_set_replicas(self.stateful_set_name(), 1)?;
        self.wait_until_healthy(Instant::now() + Duration::from_secs(60))
            .await?;

        Ok(())
    }

    fn stop(&mut self) -> Result<()> {
        info!("going to stop node {}", self.stateful_set_name());
        scale_stateful_set_replicas(self.stateful_set_name(), 0)
    }

    fn clear_storage(&mut self) -> Result<()> {
        let sts_name = self.stateful_set_name.clone();
        let pvc_name = if sts_name.contains("fullnode") {
            format!("fn-{}-0", sts_name)
        } else {
            sts_name
        };
        let delete_pvc_args = ["delete", "pvc", &pvc_name];
        info!("{:?}", delete_pvc_args);
        let cleanup_output = Command::new("kubectl")
            .stdout(Stdio::inherit())
            .args(&delete_pvc_args)
            .output()
            .expect("failed to clear node storage");
        assert!(
            cleanup_output.status.success(),
            "{}",
            String::from_utf8(cleanup_output.stderr).unwrap()
        );

        Ok(())
    }

    async fn health_check(&mut self) -> Result<(), HealthCheckError> {
        self.rest_client()
            .get_ledger_information()
            .await
            .map(|_| ())
            .map_err(|e| {
                HealthCheckError::Failure(format_err!("K8s node health_check failed: {}", e))
            })
    }

    // TODO: replace this with prometheus query?
    fn counter(&self, counter: &str, port: u64) -> Result<f64> {
        let response: Value =
            reqwest::blocking::get(format!("http://localhost:{}/counters", port))?.json()?;
        if let Value::Number(ref response) = response[counter] {
            if let Some(response) = response.as_f64() {
                Ok(response)
            } else {
                Err(format_err!(
                    "Failed to parse counter({}) as f64: {:?}",
                    counter,
                    response
                ))
            }
        } else {
            Err(format_err!(
                "Counter({}) was not a Value::Number: {:?}",
                counter,
                response[counter]
            ))
        }
    }

    // TODO: verify this still works
    fn expose_metric(&self) -> Result<u64> {
        let pod_name = format!("{}-0", self.stateful_set_name);
        let port = get_free_port() as u64;
        let port_forward_args = [
            "port-forward",
            &format!("pod/{}", pod_name),
            &format!("{}:{}", port, NODE_METRIC_PORT),
        ];
        info!("{:?}", port_forward_args);
        let _ = Command::new("kubectl")
            .stdout(Stdio::null())
            .args(&port_forward_args)
            .spawn()
            .with_context(|| format!("Error port forwarding for node {}", pod_name))?;
        thread::sleep(Duration::from_secs(5));

        Ok(port)
    }
}

impl Validator for K8sNode {}

impl FullNode for K8sNode {}

impl Debug for K8sNode {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        let host = if self.port_forward_enabled {
            LOCALHOST
        } else {
            &self.service_name
        };
        write!(f, "{} @ {}:{}", self.name, host, self.rest_api_port)
    }
}

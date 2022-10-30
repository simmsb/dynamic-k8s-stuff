// Example to listen on port 8080 locally, forwarding to port 80 in the example pod.
// Similar to `kubectl port-forward pod/example 8080:80`.
use futures::{future::try_join_all, StreamExt, TryStreamExt};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpListener,
};
use tokio_stream::wrappers::TcpListenerStream;
use tracing::*;

use color_eyre::{eyre::ContextCompat, Result};
use k8s_openapi::api::core::v1::{Namespace, Pod, Service};
use kube::{
    api::{Api, DeleteParams, PostParams},
    runtime::wait::{await_condition, conditions::is_pod_running},
    Client, ResourceExt,
};

fn install_tracing() -> Result<()> {
    use tracing_subscriber::fmt::format::FmtSpan;
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .pretty();
    let filter_layer = tracing_subscriber::EnvFilter::builder()
        .with_default_directive(concat!(env!("CARGO_CRATE_NAME"), "=debug").parse()?)
        .from_env()?;

    tracing_subscriber::registry()
        .with(tracing_error::ErrorLayer::default())
        .with(filter_layer)
        .with(fmt_layer)
        .init();

    Ok(())
}

#[allow(unused)]
struct Context {
    ns: Namespace,
    pod_api: Api<Pod>,
    ns_api: Api<Namespace>,
    svc_api: Api<Service>,
    client: Client,
}

impl Context {
    async fn get(ns: String) -> Result<Self> {
        let client = Client::try_default().await?;
        let ns_api = Api::all(client.clone());
        let pod_api = Api::namespaced(client.clone(), &ns);
        let svc_api = Api::namespaced(client.clone(), &ns);

        let ns_dat: Namespace = serde_json::from_value(serde_json::json!({
            "apiVersion": "v1",
            "kind": "Namespace",
            "metadata": { "name": ns },
        }))?;
        info!(ns, "Creating namespace");
        let ns = ns_api.create(&PostParams::default(), &ns_dat).await?;

        Ok(Self {
            ns,
            pod_api,
            ns_api,
            svc_api,
            client,
        })
    }

    fn pod_api(&self) -> Api<Pod> {
        self.pod_api.clone()
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        info!(
            ns = self.ns.name_any(),
            "Context dropping, deleting namespace"
        );
        std::thread::scope(|s| {
            s.spawn(|| {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                rt.block_on(
                    self.ns_api
                        .delete(&self.ns.name_unchecked(), &DeleteParams::default()),
                )
            })
            .join()
        })
        .unwrap()
        .unwrap(); // 🤨
    }
}

async fn create_and_wait(
    ctx: &Context,
    pods: &[Pod],
    services: &[Service],
    timeout: Duration,
) -> Result<(Vec<Pod>, Vec<Service>)> {
    let pods = try_join_all(pods.iter().map(|p| async {
        let name = p.name_unchecked();
        let pod = ctx
            .pod_api
            .create(&PostParams::default(), p)
            .instrument(info_span!("Creating pod", name))
            .await?;
        let running = await_condition(
            ctx.pod_api.clone(),
            pod.metadata.name.as_ref().unwrap(),
            is_pod_running(),
        )
        .instrument(info_span!("Waiting for pod to come up", name));
        let _ = tokio::time::timeout(timeout.clone(), running).await?;

        let pod = ctx.pod_api.get(&name).await?;

        Ok::<_, color_eyre::eyre::Error>(pod)
    }))
    .await?;

    let services = try_join_all(services.iter().map(|s| async {
        let name = s.name_unchecked();
        let service = ctx
            .svc_api
            .create(&PostParams::default(), s)
            .instrument(info_span!("Creating service", name))
            .await?;

        Ok::<_, color_eyre::eyre::Error>(service)
    }))
    .await?;

    Ok((pods, services))
}

fn nginx_pod(name: &str, uuid: &str) -> Result<Pod> {
    let pod = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": name,
            "labels": {
                "app.kubernetes.io/instance": format!("pod-{}", uuid),
            }
        },
        "spec": {
            "containers": [{
                "name": "nginx",
                "image": "lscr.io/linuxserver/nginx:latest",
            }],
        }
    }))?;

    Ok(pod)
}

fn socat_pod(name: &str, uuid: &str, upstream_name: &str) -> Result<Pod> {
    let pod = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": name,
            "labels": {
                "app.kubernetes.io/instance": format!("pod-{}", uuid),
            }
        },
        "spec": {
            "containers": [{
                "name": "socat",
                "image": "alpine/socat",
                "args": ["tcp-listen:80,fork,reuseaddr", format!("tcp-connect:{}:80", upstream_name)]
            }, {
                "name": "net",
                "image": "nicolaka/netshoot",
                "args": ["tail", "-f", "/dev/null"],
                "securityContext": {
                    "capabilities": {
                        "add": ["NET_ADMIN"]
                    }
                }
            }],
        }
    }))?;

    Ok(pod)
}

fn mk_service(name: &str, upstream_uuid: &str) -> Result<Service> {
    let pod = serde_json::from_value(serde_json::json!({
        "apiVersion": "v1",
        "kind": "Service",
        "metadata": {
            "name": name,
        },
        "spec": {
            "selector": {
                "app.kubernetes.io/instance": format!("pod-{}", upstream_uuid),
            },
            "ports": [{
                "protocol": "TCP",
                "port": 80,
                "targetPort": 80,
            }]
        }
    }))?;

    Ok(pod)
}

fn uuid() -> String {
    uuid::Uuid::new_v4().hyphenated().to_string()
}

async fn inner(ctx: &Context) -> Result<()> {
    let nginx_uuid = uuid();
    let socat_uuid = uuid();

    let nginx = nginx_pod("nginx", &nginx_uuid)?;
    let nginx_svc = mk_service("nginx-svc", &nginx_uuid)?;
    let socat = socat_pod("socat", &socat_uuid, "nginx-svc")?;

    let pods = create_and_wait(ctx, &[nginx, socat], &[nginx_svc], Duration::from_secs(120))
        .instrument(info_span!("Creating pods"))
        .await?;

    info!("Created pods: {:#?}", pods);

    let addr = SocketAddr::from(([127, 0, 0, 1], 8080));
    let pod_port = 80;

    info!(local_addr = %addr, pod_port, "forwarding traffic to the pod");

    let server = TcpListenerStream::new(TcpListener::bind(addr).await.unwrap())
        .take_until(tokio::signal::ctrl_c())
        .try_for_each(|client_conn| async {
            if let Ok(peer_addr) = client_conn.peer_addr() {
                info!(%peer_addr, "new connection");
            }
            let pod_api = ctx.pod_api();
            tokio::spawn(async move {
                if let Err(e) = forward_connection(&pod_api, "socat", 80, client_conn).await {
                    error!(
                        error = e.as_ref() as &dyn std::error::Error,
                        "failed to forward connection"
                    );
                }
            });
            // keep the server running
            Ok(())
        });

    if let Err(e) = server.await {
        error!(error = &e as &dyn std::error::Error, "server error");
    }

    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    install_tracing()?;
    let ns = format!("fflex-{}", uuid::Uuid::new_v4().hyphenated());
    let context = Context::get(ns).await?;
    inner(&context).await?;

    Ok(())
}

async fn forward_connection(
    pods: &Api<Pod>,
    pod_name: &str,
    port: u16,
    mut client_conn: impl AsyncRead + AsyncWrite + Unpin,
) -> Result<()> {
    let mut forwarder = pods.portforward(pod_name, &[port]).await?;
    let mut upstream_conn = forwarder
        .take_stream(port)
        .context("port not found in forwarder")?;
    tokio::io::copy_bidirectional(&mut client_conn, &mut upstream_conn).await?;
    drop(upstream_conn);
    forwarder.join().await?;
    info!("connection closed");
    Ok(())
}

use anyhow::Result;
use clap::{App, AppSettings, Arg};
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Client, Method, Request, Response, Server, StatusCode};
use interceptor::registry::Registry;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::time::Duration;
use webrtc::api::interceptor_registry::register_default_interceptors;
use webrtc::api::media_engine::MediaEngine;
use webrtc::api::APIBuilder;
use webrtc::data::data_channel::data_channel_message::DataChannelMessage;
use webrtc::data::data_channel::DataChannel;
use webrtc::peer::configuration::Configuration;
use webrtc::peer::ice::ice_candidate::{ICECandidate, ICECandidateInit};
use webrtc::peer::ice::ice_server::ICEServer;
use webrtc::peer::peer_connection::PeerConnection;
use webrtc::peer::peer_connection_state::PeerConnectionState;
use webrtc::peer::sdp::session_description::{SessionDescription, SessionDescriptionSerde};

#[macro_use]
extern crate lazy_static;

lazy_static! {
    static ref PEER_CONNECTION_MUTEX: Arc<Mutex<Option<Arc<PeerConnection>>>> =
        Arc::new(Mutex::new(None));
    static ref PENDING_CANDIDATES: Arc<Mutex<Vec<ICECandidate>>> = Arc::new(Mutex::new(vec![]));
    static ref OFFER_ADDR: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
}

async fn signal_candidate(addr: &str, c: &ICECandidate) -> Result<()> {
    let payload = c.to_json().await?.candidate;
    let req = match Request::builder()
        .method(Method::POST)
        .uri(format!("http://{}/candidate", addr))
        .header("content-type", "application/json; charset=utf-8")
        .body(Body::from(payload))
    {
        Ok(req) => req,
        Err(err) => {
            println!("{}", err);
            return Err(err.into());
        }
    };

    let resp = match Client::new().request(req).await {
        Ok(resp) => resp,
        Err(err) => {
            println!("{}", err);
            return Err(err.into());
        }
    };
    println!("Response: {}", resp.status());

    Ok(())
}

// HTTP Listener to get ICE Credentials/Candidate from remote Peer
async fn remote_handler(req: Request<Body>) -> Result<Response<Body>, hyper::Error> {
    let pc = {
        let pcm = PEER_CONNECTION_MUTEX.lock().await;
        pcm.clone().unwrap()
    };
    let offer_addr = {
        let offer_addr = OFFER_ADDR.lock().await;
        offer_addr.clone()
    };

    match (req.method(), req.uri().path()) {
        // A HTTP handler that allows the other WebRTC-rs or Pion instance to send us ICE candidates
        // This allows us to add ICE candidates faster, we don't have to wait for STUN or TURN
        // candidates which may be slower
        (&Method::POST, "/candidate") => {
            let candidate =
                match std::str::from_utf8(&hyper::body::to_bytes(req.into_body()).await?) {
                    Ok(s) => s.to_owned(),
                    Err(err) => panic!("{}", err),
                };

            if let Err(err) = pc
                .add_ice_candidate(ICECandidateInit {
                    candidate,
                    ..Default::default()
                })
                .await
            {
                panic!("{}", err);
            }

            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::OK;
            Ok(response)
        }

        // A HTTP handler that processes a SessionDescription given to us from the other WebRTC-rs or Pion process
        (&Method::POST, "/sdp") => {
            let mut offer = SessionDescription::default();
            let offer_str =
                match std::str::from_utf8(&hyper::body::to_bytes(req.into_body()).await?) {
                    Ok(s) => s.to_owned(),
                    Err(err) => panic!("{}", err),
                };
            offer.serde = match serde_json::from_str::<SessionDescriptionSerde>(&offer_str) {
                Ok(s) => s,
                Err(err) => panic!("{}", err),
            };

            if let Err(err) = pc.set_remote_description(offer).await {
                panic!("{}", err);
            }

            // Create an answer to send to the other process
            let answer = match pc.create_answer(None).await {
                Ok(a) => a,
                Err(err) => panic!("{}", err),
            };

            // Send our answer to the HTTP server listening in the other process
            let payload = match serde_json::to_string(&answer.serde) {
                Ok(p) => p,
                Err(err) => panic!("{}", err),
            };

            let req = match Request::builder()
                .method(Method::POST)
                .uri(format!("http://{}/candidate", offer_addr))
                .header("content-type", "application/json; charset=utf-8")
                .body(Body::from(payload))
            {
                Ok(req) => req,
                Err(err) => panic!("{}", err),
            };

            let resp = match Client::new().request(req).await {
                Ok(resp) => resp,
                Err(err) => {
                    println!("{}", err);
                    return Err(err.into());
                }
            };
            println!("Response: {}", resp.status());

            // Sets the LocalDescription, and starts our UDP listeners
            if let Err(err) = pc.set_local_description(answer).await {
                panic!("{}", err);
            }

            {
                let cs = PENDING_CANDIDATES.lock().await;
                for c in &*cs {
                    if let Err(err) = signal_candidate(&offer_addr, c).await {
                        panic!("{}", err);
                    }
                }
            }

            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::OK;
            Ok(response)
        }
        // Return the 404 Not Found for other routes.
        _ => {
            let mut not_found = Response::default();
            *not_found.status_mut() = StatusCode::NOT_FOUND;
            Ok(not_found)
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    /*env_logger::Builder::new()
    .format(|buf, record| {
        writeln!(
            buf,
            "{}:{} [{}] {} - {}",
            record.file().unwrap_or("unknown"),
            record.line().unwrap_or(0),
            record.level(),
            chrono::Local::now().format("%H:%M:%S.%6f"),
            record.args()
        )
    })
    .filter(None, log::LevelFilter::Trace)
    .init();*/

    let mut app = App::new("Answer")
        .version("0.1.0")
        .author("Rain Liu <yliu@webrtc.rs>")
        .about("An example of WebRTC-rs Answer")
        .setting(AppSettings::DeriveDisplayOrder)
        .setting(AppSettings::SubcommandsNegateReqs)
        .arg(
            Arg::with_name("FULLHELP")
                .help("Prints more detailed help information")
                .long("fullhelp"),
        )
        .arg(
            Arg::with_name("offer-address")
                .required_unless("FULLHELP")
                .takes_value(true)
                .default_value("localhost:50000")
                .long("offer-address")
                .help("Address that the Offer HTTP server is hosted on."),
        )
        .arg(
            Arg::with_name("answer-address")
                .required_unless("FULLHELP")
                .takes_value(true)
                .default_value(":60000")
                .long("answer-address")
                .help("Address that the Answer HTTP server is hosted on."),
        );

    let matches = app.clone().get_matches();

    if matches.is_present("FULLHELP") {
        app.print_long_help().unwrap();
        std::process::exit(0);
    }

    let offer_addr = matches.value_of("offer-address").unwrap().to_owned();
    let answer_addr = matches.value_of("answer-address").unwrap().to_owned();

    {
        let mut oa = OFFER_ADDR.lock().await;
        *oa = offer_addr.clone();
    }

    // Prepare the configuration
    let config = Configuration {
        ice_servers: vec![ICEServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }],
        ..Default::default()
    };

    // Create a MediaEngine object to configure the supported codec
    let mut m = MediaEngine::default();
    m.register_default_codecs()?;

    let mut registry = Registry::new();

    // Use the default set of Interceptors
    registry = register_default_interceptors(registry, &mut m)?;

    // Create the API object with the MediaEngine
    let api = APIBuilder::new()
        .with_media_engine(m)
        .with_interceptor_registry(registry)
        .build();

    // Create a new RTCPeerConnection
    let peer_connection = Arc::new(api.new_peer_connection(config).await?);

    // When an ICE candidate is available send to the other Pion instance
    // the other Pion instance will add this candidate by calling AddICECandidate
    let peer_connection2 = Arc::clone(&peer_connection);
    let pending_candidates2 = Arc::clone(&PENDING_CANDIDATES);
    let offer_addr2 = offer_addr.clone();
    peer_connection
        .on_ice_candidate(Box::new(move |c: Option<ICECandidate>| {
            let peer_connection3 = Arc::clone(&peer_connection2);
            let pending_candidates3 = Arc::clone(&pending_candidates2);
            let offer_addr3 = offer_addr2.clone();
            Box::pin(async move {
                if let Some(c) = c {
                    let desc = peer_connection3.remote_description().await;
                    if desc.is_none() {
                        let mut cs = pending_candidates3.lock().await;
                        cs.push(c);
                    } else if let Err(err) = signal_candidate(&offer_addr3, &c).await {
                        assert!(false, "{}", err);
                    }
                }
            })
        }))
        .await;

    println!("Listening on http://{}", answer_addr);
    {
        let mut pcm = PEER_CONNECTION_MUTEX.lock().await;
        *pcm = Some(Arc::clone(&peer_connection));
    }

    // Set the handler for Peer connection state
    // This will notify you when the peer has connected/disconnected
    peer_connection
        .on_peer_connection_state_change(Box::new(move |s: PeerConnectionState| {
            print!("Peer Connection State has changed: {}\n", s);

            if s == PeerConnectionState::Failed {
                // Wait until PeerConnection has had no network activity for 30 seconds or another failure. It may be reconnected using an ICE Restart.
                // Use webrtc.PeerConnectionStateDisconnected if you are interested in detecting faster timeout.
                // Note that the PeerConnection may come back from PeerConnectionStateDisconnected.
                println!("Peer Connection has gone to failed exiting");
                std::process::exit(0);
            }

            Box::pin(async {})
        }))
        .await;

    // Register data channel creation handling
    peer_connection.on_data_channel(Box::new(move |d: Arc<DataChannel>| {
        let d_label = d.label().to_owned();
        let d_id = d.id();
        print!("New DataChannel {} {}\n", d_label, d_id);

        Box::pin(async move{
            // Register channel opening handling
            let d2 =  Arc::clone(&d);
            let d_label2 = d_label.clone();
            let d_id2 = d_id.clone();
            d.on_open(Box::new(move || {
                print!("Data channel '{}'-'{}' open. Random messages will now be sent to any connected DataChannels every 5 seconds\n", d_label2, d_id2);
                Box::pin(async move {
                    let mut result = Result::<usize>::Ok(0);
                    let mut i = 0;
                    while result.is_ok() {
                        let timeout = tokio::time::sleep(Duration::from_secs(5));
                        tokio::pin!(timeout);

                        tokio::select! {
                            _ = timeout.as_mut() =>{
                                let message = format!("Sending '{}'", i);
                                i += 1;
                                result = d2.send_text(message).await;
                            }
                        };
                    }
                })
            })).await;

            // Register text message handling
            d.on_message(Box::new(move |msg: DataChannelMessage| {
               let msg_str = String::from_utf8(msg.data.to_vec()).unwrap();
               print!("Message from DataChannel '{}': '{}'\n", d_label, msg_str);
               Box::pin(async{})
           })).await;
        })
    })).await;

    tokio::spawn(async move {
        let addr = SocketAddr::from_str(&answer_addr).unwrap();
        let service =
            make_service_fn(|_| async { Ok::<_, hyper::Error>(service_fn(remote_handler)) });
        let server = Server::bind(&addr).serve(service);
        // Run this server for... forever!
        if let Err(e) = server.await {
            eprintln!("server error: {}", e);
        }
    });

    println!("Press ctlr-c to stop server");
    tokio::signal::ctrl_c().await.unwrap();

    peer_connection.close().await?;

    Ok(())
}

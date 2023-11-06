use std::{str::FromStr, collections::HashMap};

use onvif::{
    discovery::{self, Device},
    soap,
};
use schema::{self, transport};
use structopt::StructOpt;
use tracing::debug;
use url::Url;
use std::fmt;

use env_logger;
use log;

#[derive(StructOpt, Clone, Debug)]
#[structopt(name = "view", about = "Factbird View camera discovery tool")]
struct Args {
    #[structopt(global = true, long, requires = "password")]
    username: Option<String>,

    #[structopt(global = true, long, requires = "username")]
    password: Option<String>,

    #[structopt(global = true, long, default_value = "192.168.0.1")]
    listen_addr: String,
}

struct ClientArgs {
    username: Option<String>,

    password: Option<String>,

    uri: Url,

    service_path: String,
}

struct Clients {
    devicemgmt: soap::client::Client,
    event: Option<soap::client::Client>,
    deviceio: Option<soap::client::Client>,
    media: Option<soap::client::Client>,
    media2: Option<soap::client::Client>,
    imaging: Option<soap::client::Client>,
    ptz: Option<soap::client::Client>,
    analytics: Option<soap::client::Client>,
}

impl Clients {
    async fn new(args: &ClientArgs) -> Result<Self, String> {
        let creds = match (args.username.as_ref(), args.password.as_ref()) {
            (Some(username), Some(password)) => Some(soap::client::Credentials {
                username: username.clone(),
                password: password.clone(),
            }),
            (None, None) => None,
            _ => panic!("username and password must be specified together"),
        };
        let devicemgmt_uri = args.uri.join(&args.service_path).unwrap();
        let mut out = Self {
            devicemgmt: soap::client::ClientBuilder::new(&devicemgmt_uri)
                .credentials(creds.clone())
                .build(),
            imaging: None,
            ptz: None,
            event: None,
            deviceio: None,
            media: None,
            media2: None,
            analytics: None,
        };
        let services =
            schema::devicemgmt::get_services(&out.devicemgmt, &Default::default()).await?;
        for service in &services.service {
            let service_url = Url::parse(&service.x_addr).map_err(|e| e.to_string())?;
            if !service_url.as_str().starts_with(args.uri.as_str()) {
                return Err(format!(
                    "Service URI {} is not within base URI {}",
                    service_url, args.uri
                ));
            }
            let svc = Some(
                soap::client::ClientBuilder::new(&service_url)
                    .credentials(creds.clone())
                    .build(),
            );
            match service.namespace.as_str() {
                "http://www.onvif.org/ver10/device/wsdl" => {
                    if service_url != devicemgmt_uri {
                        return Err(format!(
                            "advertised device mgmt uri {} not expected {}",
                            service_url, devicemgmt_uri
                        ));
                    }
                }
                "http://www.onvif.org/ver10/events/wsdl" => out.event = svc,
                "http://www.onvif.org/ver10/deviceIO/wsdl" => out.deviceio = svc,
                "http://www.onvif.org/ver10/media/wsdl" => out.media = svc,
                "http://www.onvif.org/ver20/media/wsdl" => out.media2 = svc,
                "http://www.onvif.org/ver20/imaging/wsdl" => out.imaging = svc,
                "http://www.onvif.org/ver20/ptz/wsdl" => out.ptz = svc,
                "http://www.onvif.org/ver20/analytics/wsdl" => out.analytics = svc,
                _ => debug!("unknown service: {:?}", service),
            }
        }
        Ok(out)
    }
    
}


pub struct VideoSpec {
    encoding: String,
    width: i32,
    height: i32,
}

pub struct StreamSpec {
    name: String,
    media_uri: String,
    video: VideoSpec,
}

async fn get_stream_uris(clients: &Clients) -> Result<Vec<StreamSpec>, transport::Error> {
    log::info!("Entered get_stream_uris");
    let media_client = clients
        .media
        .as_ref()
        .ok_or_else(|| transport::Error::Other("Client media is not available".into()))?;

    log::info!("Getting all available profiles");
    let profiles = schema::media::get_profiles(media_client, &Default::default()).await?;
    log::debug!("get_profiles response: {:#?}", &profiles);
    let requests: Vec<_> = profiles
        .profiles
        .iter()
        .map(|p: &schema::onvif::Profile| schema::media::GetStreamUri {
            profile_token: schema::onvif::ReferenceToken(p.token.0.clone()),
            stream_setup: schema::onvif::StreamSetup {
                stream: schema::onvif::StreamType::RtpUnicast,
                transport: schema::onvif::Transport {
                    protocol: schema::onvif::TransportProtocol::Rtsp,
                    tunnel: vec![],
                },
            },
        })
        .collect();

    log::info!("Getting streamUri per profile");
    let mut responses = HashMap::new(); // Store responses in a Vec
    for (i, request) in requests.iter().enumerate() {
        let response = schema::media::get_stream_uri(media_client, request).await;
        match &response {
            Ok(_) => {
                log::info!("Request #{} {} succeeded", i, request.profile_token);
                responses.insert(i, response.unwrap());
            } 
            Err(err) => log::error!("Request #{} {} failed with error: {:?}", i, request.profile_token,err),
        }
        
    }

    log::info!("Copying streamUri to streams");
    let mut streams = vec![];
    for (i, p) in profiles.profiles.iter().enumerate() {
        if let Some(ref v) = p.video_encoder_configuration {
            if let Some (resp) = responses.get(&i) {
                log::info!("Stream {}: {} was successfully added.", i, p.name);

                streams.push(StreamSpec {
                    name: p.name.0.clone(),
                    media_uri: resp.media_uri.uri.clone(),
                    video: VideoSpec {
                        encoding: format!("{:?}", v.encoding),
                        width: v.resolution.width,
                        height: v.resolution.height,
                    },
                });
            }
            else {
                log::info!("Stream {}: {} was filtered out because it has no media_uri.", i, p.name);
            }
            // let media_uri = responses.get(&i).map_or("None", |resp| resp.media_uri.uri.as_str()); 
        }
        else {
            log::info!("Stream {}: {} was filtered out because it has no video encoder.", i, p.name);
        }
    }
    Ok(streams)
}

#[tokio::main]
async fn main() {
    use futures_util::stream::StreamExt;
    const MAX_CONCURRENT_JUMPERS: usize = 100;
    env_logger::init();


    let listen_addr = std::net::Ipv4Addr::from_str(Args::from_args().listen_addr.as_str()).unwrap();

    if let Ok(devices_stream) = discovery::DiscoveryBuilder::default()
        .listen_address(listen_addr.into())
        .run()
        .await
    {
        log::info!("Found device. Getting links ..");
        devices_stream
            .for_each_concurrent(MAX_CONCURRENT_JUMPERS, |addr: Device| async move {
                let args = Args::from_args();
                let service_path = String::from("onvif/device_service");

                let uri = addr
                    .urls
                    .into_iter()
                    .find(|u| {
                        u.scheme() == "https"
                            && u.host_str()
                                .map(|h| {
                                    let host_ip = std::net::Ipv4Addr::from_str(h).unwrap();
                                    host_ip.octets()[..3].eq(&listen_addr.octets()[..3])
                                })
                                .unwrap_or_default()
                    })
                    .expect("device does not have any https urls?");

                let uri = uri
                    .as_str()
                    .strip_suffix(service_path.as_str())
                    .unwrap_or_else(|| uri.as_str());

                log::info!("Creating separate Clients");
                let Ok(clients) = Clients::new(&ClientArgs {
                    username: args.username.clone(),
                    password: args.password.clone(),
                    uri: Url::from_str(uri).unwrap(),
                    service_path,
                })
                .await else {
                    return;
                };
                
                log::info!("Getting streamUri's");
                if let Ok(streams) = get_stream_uris(&clients).await {

                    log::info!("Filtering for h264 encoding");
                    for stream in streams
                        .iter()
                        .filter(|s| s.video.encoding.to_ascii_lowercase().as_str() == "h264")
                    {
                        log::info!("Name: {} ", stream.name);
                        log::info!("Media_uri: {}", stream.media_uri);
                        log::info!("Videoencoding: {} with width {} and height {}. Link:",
                            stream.video.encoding,
                            stream.video.width,
                            stream.video.height);
                        
                        println!(
                            "rtsp://{}:{}@{}",
                            args.username.clone().unwrap(),
                            args.password.clone().unwrap(),
                            stream.media_uri.strip_prefix("rtsp://").unwrap()
                        );
                        log::info!("");
                    }
                }
                else {
                    log::info!("Failed to get any streamUri'");
                }
            })
            .await;
    }
    else
    {
        log::info!("Didn't find any devices.");
    }
}

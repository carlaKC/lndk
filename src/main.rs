use bitcoin::bech32::u5;
use bitcoin::secp256k1::ecdh::SharedSecret;
use bitcoin::secp256k1::ecdsa::{RecoverableSignature, Signature};
use bitcoin::secp256k1::{self, PublicKey, Scalar, Secp256k1};
use futures::executor::block_on;
use lightning::chain::keysinterface::{EntropySource, KeyMaterial, NodeSigner, Recipient};
use lightning::ln::features::InitFeatures;
use lightning::ln::msgs::UnsignedGossipMessage;
use lightning::ln::msgs::{Init, OnionMessageHandler};
use lightning::ln::peer_handler::IgnoringMessageHandler;
use lightning::onion_message::OnionMessenger;
use lightning::util::logger::{Level, Logger, Record};
use log::{debug, error, info, trace, warn};
use std::cell::RefCell;
use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::Read;
use std::marker::Copy;
use std::str::FromStr;
use std::sync::mpsc::{channel, Receiver, SendError, Sender};
use std::thread;
use tonic_lnd::tonic::Status;
use tonic_lnd::{Client, ConnectError};

#[tokio::main]
async fn main() {
    simple_logger::init_with_level(log::Level::Info).unwrap();

    let args = match parse_args() {
        Ok(args) => args,
        Err(args) => panic!("Bad arguments: {args}"),
    };

    let mut client = get_lnd_client(args).expect("failed to connect");

    let info = block_on(
        client
            .lightning()
            .get_info(tonic_lnd::lnrpc::GetInfoRequest {}),
    )
    .expect("failed to get info");

    let pubkey = PublicKey::from_str(&info.into_inner().identity_pubkey).unwrap();
    info!("Starting lndk for node: {pubkey}");

    // Setup channels that we'll use to communicate onion messenger events.
    let (sender, receiver) = channel();

    // On startup, we want to get a list of our currently online peers to notify the onion messenger that they are
    // connected. This sets up our "start state" for the messenger correctly.
    let list_peers = block_on(
        client
            .lightning()
            .list_peers(tonic_lnd::lnrpc::ListPeersRequest {
                latest_error: false,
            }),
    )
    .expect("list peers failed")
    .into_inner();

    for peer in list_peers.peers {
        let event = MessengerEvents::PeerConnected(
            PublicKey::from_str(&peer.pub_key).unwrap(),
            peer.inbound,
        );

        match sender.send(event) {
            Ok(_) => debug!("startup peer events sent: {event}"),
            Err(err) => panic!("could not send peer event on startup {event}: {err}"),
        };
    }

    // Create an onion messenger that depends on LND's signer client and consume related events.
    let mut node_signer_client = client.signer().clone();
    let messenger_events_handler = thread::spawn(move || {
        let node_signer = LndNodeSigner::new(pubkey, &mut node_signer_client);

        let messenger_utils = MessengerUtilities {};
        let onion_messenger = OnionMessenger::new(
            &messenger_utils,
            &node_signer,
            &messenger_utils,
            IgnoringMessageHandler {},
        );

        match consume_messenger_events(onion_messenger, receiver) {
            Ok(_) => debug!("Onion message events consumer exited"),
            Err(e) => error!("Onion message events consumer exited: {e}"),
        };
    });

    messenger_events_handler.join().unwrap();
}

#[derive(Debug)]
enum MessengerError {
    SendError(SendError<MessengerEvents>),
    StreamError(MessengerEvents),
}

impl Error for MessengerError {}

impl fmt::Display for MessengerError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            MessengerError::SendError(e) => write!(f, "Error sending messenger event: {e}"),
            MessengerError::StreamError(s) => write!(f, "LND stream {s} exited"),
        }
    }
}

trait PeerEventProducer {
    fn receive(&mut self) -> Result<tonic_lnd::lnrpc::PeerEvent, Status>;
}

fn produce_peer_events(
    mut source: impl PeerEventProducer,
    events: Sender<MessengerEvents>,
) -> Result<(), MessengerError> {
    loop {
        match source.receive() {
            Ok(peer_event) => match peer_event.r#type() {
                tonic_lnd::lnrpc::peer_event::EventType::PeerOnline => {
                    // TODO: need to lookup whether the peer supports onion messaging (describegraph?) to provide input
                    // here.
                    let event = MessengerEvents::PeerConnected(
                        PublicKey::from_str(&peer_event.pub_key).unwrap(),
                        true,
                    );
                    match events.send(event) {
                        Ok(_) => debug!("peer events sent: {event}"),
                        Err(err) => return Err(MessengerError::SendError(err)),
                    };
                }
                tonic_lnd::lnrpc::peer_event::EventType::PeerOffline => {
                    let event = MessengerEvents::PeerDisconnected(
                        PublicKey::from_str(&peer_event.pub_key).unwrap(),
                    );
                    match events.send(event) {
                        Ok(_) => debug!("peer events sent: {event}"),
                        Err(err) => return Err(MessengerError::SendError(err)),
                    };
                }
            },
            Err(s) => {
                debug!("peer events receive failed: {s}");

                let event = MessengerEvents::ProducerExit(Producer::PeerEvents);
                match events.send(event) {
                    Ok(_) => debug!("peer events sent: {event}"),
                    Err(err) => error!("peer events: send producer exit failed: {err}"),
                }

                return Err(MessengerError::StreamError(event));
            }
        }
    }
}

#[derive(Debug, Copy, Clone)]
enum Producer {
    PeerEvents,
}

impl fmt::Display for Producer {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Producer::PeerEvents => write!(f, "Peer events"),
        }
    }
}

// MessengerEvents represents all of the events that are relevant to onion messages.
#[derive(Debug, Copy, Clone)]
enum MessengerEvents {
    PeerConnected(PublicKey, bool),
    PeerDisconnected(PublicKey),
    ProducerExit(Producer),
}

impl fmt::Display for MessengerEvents {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            MessengerEvents::PeerConnected(p, o) => {
                write!(f, "{p} connected, onion message support: {o}")
            }
            MessengerEvents::PeerDisconnected(p) => write!(f, "{p} disconnected"),
            MessengerEvents::ProducerExit(s) => write!(f, "Producer {s} exited"),
        }
    }
}

// consume_messenger_events receives a series of events and delivers them to the onion messenger provided.
fn consume_messenger_events(
    onion_messenger: impl OnionMessageHandler,
    events: Receiver<MessengerEvents>,
) -> Result<(), String> {
    loop {
        match events.recv() {
            Ok(onion_event) => match onion_event {
                MessengerEvents::PeerConnected(pubkey, onion_support) => {
                    let init_features = if onion_support {
                        let onion_message_optional: u64 = 1 << 39;
                        InitFeatures::from_le_bytes(onion_message_optional.to_le_bytes().to_vec())
                    } else {
                        InitFeatures::empty()
                    };

                    if onion_messenger
                        .peer_connected(
                            &pubkey,
                            &Init {
                                features: init_features,
                                remote_network_address: None,
                            },
                            false,
                        )
                        .is_err()
                    {
                        return Err("peer connection failed".to_string());
                    };
                }
                MessengerEvents::PeerDisconnected(pubkey) => {
                    onion_messenger.peer_disconnected(&pubkey)
                }
                MessengerEvents::ProducerExit(e) => return Err(format!("upstream exit: {e}")),
            },
            Err(e) => return Err(format!("message consumer failed: {e}")),
        };
    }
}

struct LndNodeSigner<'a> {
    pubkey: PublicKey,
    secp_ctx: Secp256k1<secp256k1::All>,
    signer: RefCell<&'a mut tonic_lnd::SignerClient>,
}

impl<'a> LndNodeSigner<'a> {
    fn new(pubkey: PublicKey, signer: &'a mut tonic_lnd::SignerClient) -> Self {
        LndNodeSigner {
            pubkey,
            secp_ctx: Secp256k1::new(),
            signer: RefCell::new(signer),
        }
    }
}

impl<'a> NodeSigner for LndNodeSigner<'a> {
    /// Get node id based on the provided [`Recipient`].
    ///
    /// This method must return the same value each time it is called with a given [`Recipient`]
    /// parameter.
    ///
    /// Errors if the [`Recipient`] variant is not supported by the implementation.
    fn get_node_id(&self, recipient: Recipient) -> Result<PublicKey, ()> {
        match recipient {
            Recipient::Node => Ok(self.pubkey),
            Recipient::PhantomNode => Err(()),
        }
    }

    /// Gets the ECDH shared secret of our node secret and `other_key`, multiplying by `tweak` if
    /// one is provided. Note that this tweak can be applied to `other_key` instead of our node
    /// secret, though this is less efficient.
    ///
    /// Errors if the [`Recipient`] variant is not supported by the implementation.
    fn ecdh(
        &self,
        recipient: Recipient,
        other_key: &PublicKey,
        tweak: Option<&Scalar>,
    ) -> Result<SharedSecret, ()> {
        match recipient {
            Recipient::Node => {}
            Recipient::PhantomNode => return Err(()),
        }

        // Clone other_key so that we can tweak it (if a tweak is required). We choose to tweak the
        // `other_key` because LND's API accept a tweak parameter (so we can't tweak our secret).
        let tweaked_key = if let Some(tweak) = tweak {
            other_key.mul_tweak(&self.secp_ctx, tweak).map_err(|_| ())?
        } else {
            *other_key
        };

        let shared_secret = match block_on(self.signer.borrow_mut().derive_shared_key(
            tonic_lnd::signrpc::SharedKeyRequest {
                ephemeral_pubkey: tweaked_key.serialize().into_iter().collect::<Vec<u8>>(),
                key_desc: None,
                ..Default::default()
            },
        )) {
            Ok(shared_key_resp) => shared_key_resp.into_inner().shared_key,
            Err(_) => return Err(()),
        };

        match SharedSecret::from_slice(&shared_secret) {
            Ok(secret) => Ok(secret),
            Err(_) => Err(()),
        }
    }

    fn get_inbound_payment_key_material(&self) -> KeyMaterial {
        unimplemented!("not required for onion messaging");
    }

    fn sign_invoice(
        &self,
        _hrp_bytes: &[u8],
        _invoice_data: &[u5],
        _recipient: Recipient,
    ) -> Result<RecoverableSignature, ()> {
        unimplemented!("not required for onion messaging");
    }

    fn sign_gossip_message(&self, _msg: UnsignedGossipMessage) -> Result<Signature, ()> {
        unimplemented!("not required for onion messaging");
    }
}

// MessengerUtilities implements some utilites required for onion messenging.
struct MessengerUtilities {}

impl EntropySource for MessengerUtilities {
    fn get_secure_random_bytes(&self) -> [u8; 32] {
        let mut f = File::open("/dev/urandom").unwrap();
        let mut buf = [0u8; 32];
        f.read_exact(&mut buf).unwrap();
        buf
    }
}

impl Logger for MessengerUtilities {
    fn log(&self, record: &Record) {
        let args_str = record.args.to_string();
        match record.level {
            Level::Gossip => {}
            Level::Trace => trace!("{}", args_str),
            Level::Debug => debug!("{}", args_str),
            Level::Info => info!("{}", args_str),
            Level::Warn => warn!("{}", args_str),
            Level::Error => error!("{}", args_str),
        }
    }
}

fn get_lnd_client(cfg: LndCfg) -> Result<Client, ConnectError> {
    block_on(tonic_lnd::connect(cfg.address, cfg.cert, cfg.macaroon))
}

#[derive(Debug)]
enum ArgsError {
    NoArgs,
    AddressRequired,
    CertRequired,
    MacaroonRequired,
}

impl Error for ArgsError {}

impl fmt::Display for ArgsError {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            ArgsError::NoArgs => write!(f, "No command line arguments provided."),
            ArgsError::AddressRequired => write!(f, "LND's RPC server address is required."),
            ArgsError::CertRequired => write!(f, "Path to LND's tls certificate is required."),
            ArgsError::MacaroonRequired => write!(f, "Path to LND's macaroon is required."),
        }
    }
}

struct LndCfg {
    address: String,
    cert: String,
    macaroon: String,
}

impl LndCfg {
    fn new(address: String, cert: String, macaroon: String) -> LndCfg {
        LndCfg {
            address,
            cert,
            macaroon,
        }
    }
}

fn parse_args() -> Result<LndCfg, ArgsError> {
    let mut args = std::env::args_os();
    if args.next().is_none() {
        return Err(ArgsError::NoArgs);
    }

    let address = match args.next() {
        Some(arg) => arg.into_string().expect("address is not UTF-8"),
        None => return Err(ArgsError::AddressRequired),
    };

    let cert_file = match args.next() {
        Some(arg) => arg.into_string().expect("cert is not UTF-8"),
        None => return Err(ArgsError::CertRequired),
    };

    let macaroon_file = match args.next() {
        Some(arg) => arg.into_string().expect("macaroon is not UTF-8"),
        None => return Err(ArgsError::MacaroonRequired),
    };

    Ok(LndCfg::new(address, cert_file, macaroon_file))
}

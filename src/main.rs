use bitcoin::bech32::u5;
use bitcoin::secp256k1::ecdh::SharedSecret;
use bitcoin::secp256k1::ecdsa::{RecoverableSignature, Signature};
use bitcoin::secp256k1::{self, PublicKey, Scalar, Secp256k1};
use futures::executor::block_on;
use lightning::chain::keysinterface::{KeyMaterial, NodeSigner, Recipient};
use lightning::ln::msgs::UnsignedGossipMessage;
use std::cell::RefCell;
use std::error::Error;
use std::fmt;
use std::str::FromStr;
use tonic_lnd::{tonic::Code, Client, ConnectError};

const ONION_FEATURE_BIT: u32 = 39;
const SERVICE_UNIMPLEMENTED: &str = "Peer or sign service is unimplemented. Remember to
	 enable the peerrpc/signrpc services when building LND with make tags='peerrpc signrpc'";

#[tokio::main]
async fn main() {
    let args = match parse_args() {
        Ok(args) => args,
        Err(args) => panic!("Bad arguments: {args}"),
    };

    let mut client = get_lnd_client(args).expect("failed to connect");
    let mut client_clone = client.clone();

    let info = client
        .lightning()
        .get_info(tonic_lnd::lnrpc::GetInfoRequest {})
        .await
        .expect("failed to get info");

    let info_clone = info.into_inner().clone();
    let info_clone_2 = info_clone.clone();
    if !info_clone.features.contains_key(&ONION_FEATURE_BIT) {
        set_feature_bit(client).await;
    }

    let pubkey = PublicKey::from_str(&info_clone_2.identity_pubkey).unwrap();
    println!("Starting lndk for node: {pubkey}");

    let _node_signer = LndNodeSigner::new(pubkey, client_clone.signer());
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

/// Sets the onion messaging feature bit (described in this PR:
/// https://github.com/lightning/bolts/pull/759/), to signal that we support
/// onion messaging. This needs to be done every time we start up, because LND
/// does not currently persist the custom feature bits that are set via the RPC.
async fn set_feature_bit(mut client: Client) {
    let onion_feature_bit = 39;
    let update = tonic_lnd::peersrpc::UpdateFeatureAction {
        action: i32::from(tonic_lnd::peersrpc::UpdateAction::Add),
        feature_bit: onion_feature_bit,
    };
    let feature_updates = vec![update];
    let address_updates = vec![];

    let resp = client
        .peers()
        .update_node_announcement(tonic_lnd::peersrpc::NodeAnnouncementUpdateRequest {
            feature_updates,
            color: String::from(""),
            alias: String::from(""),
            address_updates,
        })
        .await;

    match resp {
        Ok(_) => {
            println!("Now setting the onion messaging feature bit...")
        }
        Err(status) => {
            if status.code() == Code::Unimplemented {
                panic!("{}", SERVICE_UNIMPLEMENTED);
            }

            panic!("error updating node announcement {}", status);
        }
    };

    // Call get_info again to check the bit was actually set.
    let info = client
        .lightning()
        .get_info(tonic_lnd::lnrpc::GetInfoRequest {})
        .await
        .expect("failed to get info");

    if !info.into_inner().features.contains_key(&ONION_FEATURE_BIT) {
        panic!("onion messaging feature bit failed to be set")
    }

    println!("Successfully set onion messaging bit");
}

use std::collections::HashMap;
use std::net::SocketAddr;
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use bitcoincore_rpc::bitcoin::Amount;
use bitcoincore_rpc::RpcApi;
use hyper::service::{make_service_fn, service_fn};
use hyper::{Body, Method, Request, Response, Server, StatusCode};
use payjoin::bitcoin::psbt::Psbt;
use payjoin::bitcoin::{self};
use payjoin::receive::{PayjoinProposal, UncheckedProposal};
use payjoin::{Error, PjUriBuilder, Uri, UriExt};

use super::config::AppConfig;
use super::App as AppTrait;
use crate::app::{http_agent, try_contributing_inputs, Headers};
use crate::db::Database;
#[cfg(feature = "danger-local-https")]
pub const LOCAL_CERT_FILE: &str = "localhost.der";

#[derive(Clone)]
pub(crate) struct App {
    config: AppConfig,
    db: Arc<Database>,
}

#[async_trait::async_trait]
impl AppTrait for App {
    fn new(config: AppConfig) -> Result<Self> {
        let db = Arc::new(Database::create(&config.db_path)?);
        let app = Self { config, db };
        app.bitcoind()?
            .get_blockchain_info()
            .context("Failed to connect to bitcoind. Check config RPC connection.")?;
        Ok(app)
    }

    fn bitcoind(&self) -> Result<bitcoincore_rpc::Client> {
        match &self.config.bitcoind_cookie {
            Some(cookie) => bitcoincore_rpc::Client::new(
                self.config.bitcoind_rpchost.as_str(),
                bitcoincore_rpc::Auth::CookieFile(cookie.into()),
            ),
            None => bitcoincore_rpc::Client::new(
                self.config.bitcoind_rpchost.as_str(),
                bitcoincore_rpc::Auth::UserPass(
                    self.config.bitcoind_rpcuser.clone(),
                    self.config.bitcoind_rpcpassword.clone(),
                ),
            ),
        }
        .with_context(|| "Failed to connect to bitcoind")
    }

    async fn send_payjoin(&self, bip21: &str, fee_rate: &f32) -> Result<()> {
        let uri =
            Uri::try_from(bip21).map_err(|e| anyhow!("Failed to create URI from BIP21: {}", e))?;
        let uri = uri.assume_checked();
        let uri = uri.check_pj_supported().map_err(|_| anyhow!("URI does not support Payjoin"))?;
        let (req, ctx) = self.create_pj_request(&uri, fee_rate)?.extract_v1()?;
        let http = http_agent()?;
        let body = String::from_utf8(req.body.clone()).unwrap();
        println!("Sending fallback request to {}", &req.url);
        let response = http
            .post(req.url)
            .header("Content-Type", payjoin::V1_REQ_CONTENT_TYPE)
            .body(body.clone())
            .send()
            .await
            .with_context(|| "HTTP request failed")?;
        let fallback_tx = Psbt::from_str(&body)
            .map_err(|e| anyhow!("Failed to load PSBT from base64: {}", e))?
            .extract_tx();
        println!("Sent fallback transaction txid: {}", fallback_tx.txid());
        println!(
            "Sent fallback transaction hex: {:#}",
            payjoin::bitcoin::consensus::encode::serialize_hex(&fallback_tx)
        );
        let psbt = ctx.process_response(&mut response.bytes().await?.to_vec().as_slice()).map_err(
            |e| {
                log::debug!("Error processing response: {:?}", e);
                anyhow!("Failed to process response {}", e)
            },
        )?;

        self.process_pj_response(psbt)?;
        Ok(())
    }

    async fn receive_payjoin(self, amount_arg: &str) -> Result<()> {
        let pj_uri_string = self.construct_payjoin_uri(amount_arg, None)?;
        println!(
            "Listening at {}. Configured to accept payjoin at BIP 21 Payjoin Uri:",
            self.config.port
        );
        println!("{}", pj_uri_string);

        self.start_http_server().await?;
        Ok(())
    }
}

impl App {
    fn construct_payjoin_uri(
        &self,
        amount_arg: &str,
        fallback_target: Option<&str>,
    ) -> Result<String> {
        let pj_receiver_address = self.bitcoind()?.get_new_address(None, None)?.assume_checked();
        let amount = Amount::from_sat(amount_arg.parse()?);
        let pj_part = match fallback_target {
            Some(target) => target,
            None => self.config.pj_endpoint.as_str(),
        };
        let pj_part = payjoin::Url::parse(pj_part)
            .map_err(|e| anyhow!("Failed to parse pj_endpoint: {}", e))?;

        let pj_uri = PjUriBuilder::new(pj_receiver_address, pj_part).amount(amount).build();

        Ok(pj_uri.to_string())
    }

    async fn start_http_server(self) -> Result<()> {
        let addr = SocketAddr::from(([0, 0, 0, 0], self.config.port));
        #[cfg(feature = "danger-local-https")]
        let server = {
            use std::io::Write;

            use hyper::server::conn::AddrIncoming;
            use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

            let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])?;
            let cert_der = cert.serialize_der()?;
            let mut local_cert_path = std::env::temp_dir();
            local_cert_path.push(LOCAL_CERT_FILE);
            let mut file = std::fs::File::create(local_cert_path)?;
            file.write_all(&cert_der)?;
            let key =
                PrivateKeyDer::from(PrivatePkcs8KeyDer::from(cert.serialize_private_key_der()));
            let certs = vec![CertificateDer::from(cert.serialize_der()?)];
            let incoming = AddrIncoming::bind(&addr)?;
            let acceptor = hyper_rustls::TlsAcceptor::builder()
                .with_single_cert(certs, key)
                .map_err(|e| anyhow::anyhow!("TLS error: {}", e))?
                .with_all_versions_alpn()
                .with_incoming(incoming);
            Server::builder(acceptor)
        };

        #[cfg(not(feature = "danger-local-https"))]
        let server = Server::bind(&addr);
        let app = self.clone();
        let make_svc = make_service_fn(|_| {
            let app = app.clone();
            async move {
                let handler = move |req| app.clone().handle_web_request(req);
                Ok::<_, hyper::Error>(service_fn(handler))
            }
        });
        server.serve(make_svc).await?;
        Ok(())
    }

    async fn handle_web_request(self, req: Request<Body>) -> Result<Response<Body>> {
        log::debug!("Received request: {:?}", req);
        let mut response = match (req.method(), req.uri().path()) {
            (&Method::GET, "/bip21") => {
                let query_string = req.uri().query().unwrap_or("");
                log::debug!("{:?}, {:?}", req.method(), query_string);
                let query_params: HashMap<_, _> =
                    url::form_urlencoded::parse(query_string.as_bytes()).into_owned().collect();
                let amount = query_params.get("amount").map(|amt| {
                    Amount::from_btc(amt.parse().expect("Failed to parse amount")).unwrap()
                });
                self.handle_get_bip21(amount)
                    .map_err(|e| {
                        log::error!("Error handling request: {}", e);
                        Response::builder().status(500).body(Body::from(e.to_string())).unwrap()
                    })
                    .unwrap_or_else(|err_resp| err_resp)
            }
            (&Method::POST, _) => self
                .handle_payjoin_post(req)
                .await
                .map_err(|e| match e {
                    Error::BadRequest(e) => {
                        log::error!("Error handling request: {}", e);
                        Response::builder().status(400).body(Body::from(e.to_string())).unwrap()
                    }
                    e => {
                        log::error!("Error handling request: {}", e);
                        Response::builder().status(500).body(Body::from(e.to_string())).unwrap()
                    }
                })
                .unwrap_or_else(|err_resp| err_resp),
            _ => Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Body::from("Not found"))
                .unwrap(),
        };
        response
            .headers_mut()
            .insert("Access-Control-Allow-Origin", hyper::header::HeaderValue::from_static("*"));
        Ok(response)
    }

    fn handle_get_bip21(&self, amount: Option<Amount>) -> Result<Response<Body>, Error> {
        let address = self
            .bitcoind()
            .map_err(|e| Error::Server(e.into()))?
            .get_new_address(None, None)
            .map_err(|e| Error::Server(e.into()))?
            .assume_checked();
        let uri_string = if let Some(amount) = amount {
            format!(
                "{}?amount={}&pj={}",
                address.to_qr_uri(),
                amount.to_btc(),
                self.config.pj_endpoint
            )
        } else {
            format!("{}?pj={}", address.to_qr_uri(), self.config.pj_endpoint)
        };
        let uri = payjoin::Uri::try_from(uri_string.clone())
            .map_err(|_| Error::Server(anyhow!("Could not parse payjoin URI string.").into()))?;
        let _ = uri.assume_checked(); // we just got it from bitcoind above

        Ok(Response::new(Body::from(uri_string)))
    }

    async fn handle_payjoin_post(&self, req: Request<Body>) -> Result<Response<Body>, Error> {
        let (parts, body) = req.into_parts();
        let headers = Headers(&parts.headers);
        let query_string = parts.uri.query().unwrap_or("");
        let body = std::io::Cursor::new(
            hyper::body::to_bytes(body).await.map_err(|e| Error::Server(e.into()))?.to_vec(),
        );
        let proposal =
            payjoin::receive::UncheckedProposal::from_request(body, query_string, headers)?;

        let payjoin_proposal = self.process_v1_proposal(proposal)?;
        let psbt = payjoin_proposal.psbt();
        let body = psbt.to_string();
        println!("Responded with Payjoin proposal {}", psbt.clone().extract_tx().txid());
        Ok(Response::new(Body::from(body)))
    }

    fn process_v1_proposal(&self, proposal: UncheckedProposal) -> Result<PayjoinProposal, Error> {
        let bitcoind = self.bitcoind().map_err(|e| Error::Server(e.into()))?;

        // in a payment processor where the sender could go offline, this is where you schedule to broadcast the original_tx
        let _to_broadcast_in_failure_case = proposal.extract_tx_to_schedule_broadcast();

        // The network is used for checks later
        let network = bitcoind
            .get_blockchain_info()
            .map_err(|e| Error::Server(e.into()))
            .and_then(|info| {
                bitcoin::Network::from_core_arg(&info.chain).map_err(|e| Error::Server(e.into()))
            })?;

        // Receive Check 1: Can Broadcast
        let proposal = proposal.check_broadcast_suitability(None, |tx| {
            let raw_tx = bitcoin::consensus::encode::serialize_hex(&tx);
            let mempool_results =
                bitcoind.test_mempool_accept(&[raw_tx]).map_err(|e| Error::Server(e.into()))?;
            match mempool_results.first() {
                Some(result) => Ok(result.allowed),
                None => Err(Error::Server(
                    anyhow!("No mempool results returned on broadcast check").into(),
                )),
            }
        })?;
        log::trace!("check1");

        // Receive Check 2: receiver can't sign for proposal inputs
        let proposal = proposal.check_inputs_not_owned(|input| {
            if let Ok(address) = bitcoin::Address::from_script(input, network) {
                bitcoind
                    .get_address_info(&address)
                    .map(|info| info.is_mine.unwrap_or(false))
                    .map_err(|e| Error::Server(e.into()))
            } else {
                Ok(false)
            }
        })?;
        log::trace!("check2");
        // Receive Check 3: receiver can't sign for proposal inputs
        let proposal = proposal.check_no_mixed_input_scripts()?;
        log::trace!("check3");

        // Receive Check 4: have we seen this input before? More of a check for non-interactive i.e. payment processor receivers.
        let payjoin = proposal.check_no_inputs_seen_before(|input| {
            self.db.insert_input_seen_before(*input).map_err(|e| Error::Server(e.into()))
        })?;
        log::trace!("check4");

        let mut provisional_payjoin = payjoin.identify_receiver_outputs(|output_script| {
            if let Ok(address) = bitcoin::Address::from_script(output_script, network) {
                bitcoind
                    .get_address_info(&address)
                    .map(|info| info.is_mine.unwrap_or(false))
                    .map_err(|e| Error::Server(e.into()))
            } else {
                Ok(false)
            }
        })?;

        let mut provisional_payjoin = provisional_payjoin.try_substitute_receiver_output(|| {
            Ok(bitcoind
                .get_new_address(None, None)
                .map_err(|e| Error::Server(e.into()))?
                .require_network(network)
                .map_err(|e| Error::Server(e.into()))?
                .script_pubkey())
        })?;

        _ = try_contributing_inputs(&mut provisional_payjoin, &bitcoind)
            .map_err(|e| log::warn!("Failed to contribute inputs: {}", e));

        let payjoin_proposal = provisional_payjoin.provisional_proposal().finalize_proposal(
            |psbt: &Psbt| {
                bitcoind
                    .wallet_process_psbt(&psbt.to_string(), None, None, Some(false))
                    .map(|res| Psbt::from_str(&res.psbt).map_err(|e| Error::Server(e.into())))
                    .map_err(|e| Error::Server(e.into()))?
            },
            Some(bitcoin::FeeRate::MIN),
        )?;
        let payjoin_proposal_psbt = payjoin_proposal.psbt();
        println!(
            "Responded with Payjoin proposal {}",
            payjoin_proposal_psbt.clone().extract_tx().txid()
        );
        Ok(payjoin_proposal)
    }
}

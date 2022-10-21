use arrayvec::ArrayVec;
use ffi::{setup_tls_info, setup_ulp, KtlsCompatibilityError};
use futures::future::join_all;
use rustls::{Connection, ConnectionTrafficSecrets, SupportedCipherSuite};
use smallvec::SmallVec;
use std::{
    io,
    net::SocketAddr,
    os::unix::prelude::{AsRawFd, RawFd},
    pin::Pin,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
};

mod ffi;
use crate::ffi::CryptoInfo;

mod ktls_stream;
pub use ktls_stream::KtlsStream;

#[derive(Debug, Default)]
pub struct CompatibleCiphers {
    pub tls12: CompatibleCiphersForVersion,
    pub tls13: CompatibleCiphersForVersion,
}

#[derive(Debug, Default)]
pub struct CompatibleCiphersForVersion {
    pub aes_gcm_128: bool,
    pub aes_gcm_256: bool,
    pub chacha20_poly1305: bool,
}

impl CompatibleCiphers {
    const CIPHERS_COUNT: usize = 6;

    /// List compatible ciphers. This listens on a TCP socket and blocks for a
    /// little while. Do once at the very start of a program. Should probably be
    /// behind a lazy_static / once_cell
    pub async fn new() -> io::Result<Self> {
        let mut ciphers = CompatibleCiphers::default();

        let ln = TcpListener::bind("0.0.0.0:0").await?;
        let local_addr = ln.local_addr()?;

        // socks to the ln
        let mut socks: ArrayVec<TcpStream, { Self::CIPHERS_COUNT }> = ArrayVec::new();
        // Accepted conns of ln
        let mut accepted_conns: ArrayVec<TcpStream, { Self::CIPHERS_COUNT }> = ArrayVec::new();

        let mut new_accepted_conns: SmallVec<[(TcpStream, SocketAddr); 8]> = SmallVec::new();

        for _ in 0..Self::CIPHERS_COUNT {
            async fn accept_conns(
                ln: &TcpListener,
                new_accepted_conns: &mut SmallVec<[(TcpStream, SocketAddr); 8]>,
            ) {
                loop {
                    if let Ok(conn) = ln.accept().await {
                        new_accepted_conns.push(conn);
                    }
                }
            }

            let sock = tokio::select! {
                _ = accept_conns(&ln, &mut new_accepted_conns) => unreachable!(),
                res = TcpStream::connect(local_addr) => res?,
            };

            // Filter out new_accepted_conns
            let addr = sock.local_addr()?;

            accepted_conns.extend(
                new_accepted_conns
                    .drain(0..new_accepted_conns.len())
                    .filter_map(|(new_accepted_conn, remote_addr)| {
                        (remote_addr == addr).then_some(new_accepted_conn)
                    }),
            );

            socks.push(sock);
        }

        ciphers.test_ciphers((&*socks).try_into().unwrap()).await;

        Ok(ciphers)
    }

    async fn test_ciphers(&mut self, socks: &[TcpStream; Self::CIPHERS_COUNT]) {
        async fn test_cipher(
            cipher_suite: SupportedCipherSuite,
            field: &mut bool,
            sock: &TcpStream,
        ) {
            *field = sample_cipher_setup(sock, cipher_suite).await.is_ok()
        }

        let ciphers = [
            (
                rustls::cipher_suite::TLS13_AES_128_GCM_SHA256,
                &mut self.tls13.aes_gcm_128,
            ),
            (
                rustls::cipher_suite::TLS13_AES_256_GCM_SHA384,
                &mut self.tls13.aes_gcm_256,
            ),
            (
                rustls::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
                &mut self.tls13.chacha20_poly1305,
            ),
            (
                rustls::cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
                &mut self.tls12.aes_gcm_128,
            ),
            (
                rustls::cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
                &mut self.tls12.aes_gcm_256,
            ),
            (
                rustls::cipher_suite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
                &mut self.tls12.chacha20_poly1305,
            ),
        ];

        assert_eq!(ciphers.len(), Self::CIPHERS_COUNT);

        join_all(
            ciphers
                .into_iter()
                .zip(socks)
                .map(|((cipher_suite, field), sock)| test_cipher(cipher_suite, field, sock)),
        )
        .await;
    }

    /// Returns true if we're reasonably confident that functions like
    /// [config_ktls_client] and [config_ktls_server] will succeed.
    pub fn is_compatible(&self, suite: &SupportedCipherSuite) -> bool {
        let (fields, bulk) = match suite {
            SupportedCipherSuite::Tls12(suite) => (&self.tls12, &suite.common.bulk),
            SupportedCipherSuite::Tls13(suite) => (&self.tls13, &suite.common.bulk),
        };
        match bulk {
            rustls::BulkAlgorithm::Aes128Gcm => fields.aes_gcm_128,
            rustls::BulkAlgorithm::Aes256Gcm => fields.aes_gcm_256,
            rustls::BulkAlgorithm::Chacha20Poly1305 => fields.chacha20_poly1305,
        }
    }
}

async fn sample_cipher_setup(
    sock: &TcpStream,
    cipher_suite: SupportedCipherSuite,
) -> Result<(), Error> {
    let bulk_algo = match cipher_suite {
        SupportedCipherSuite::Tls12(suite) => &suite.common.bulk,
        SupportedCipherSuite::Tls13(suite) => &suite.common.bulk,
    };
    let zero_secrets = match bulk_algo {
        rustls::BulkAlgorithm::Aes128Gcm => ConnectionTrafficSecrets::Aes128Gcm {
            key: Default::default(),
            salt: Default::default(),
            iv: Default::default(),
        },
        rustls::BulkAlgorithm::Aes256Gcm => ConnectionTrafficSecrets::Aes256Gcm {
            key: Default::default(),
            salt: Default::default(),
            iv: Default::default(),
        },
        rustls::BulkAlgorithm::Chacha20Poly1305 => ConnectionTrafficSecrets::Chacha20Poly1305 {
            key: Default::default(),
            iv: Default::default(),
        },
    };

    let seq_secrets = (0, zero_secrets);
    let info = CryptoInfo::from_rustls(cipher_suite, seq_secrets).unwrap();

    let fd = sock.as_raw_fd();

    setup_ulp(fd).map_err(Error::UlpError)?;

    setup_tls_info(fd, ffi::Direction::Tx, info)?;

    Ok(())
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("failed to connect to the tcp socket: {0}")]
    ConnectionError(#[source] std::io::Error),

    #[error("failed to enable TLS ULP (upper level protocol): {0}")]
    UlpError(#[source] std::io::Error),

    #[error("kTLS compatibility error: {0}")]
    KtlsCompatibility(#[from] KtlsCompatibilityError),

    #[error("failed to export secrets")]
    ExportSecrets(#[source] rustls::Error),

    #[error("failed to configure tx/rx (unsupported cipher?): {0}")]
    TlsCryptoInfoError(#[source] std::io::Error),

    #[error("no negotiated cipher suite: call config_ktls_* only /after/ the handshake")]
    NoNegotiatedCipherSuite,
}

/// Configure kTLS for this socket. If this call succeeds, data can be
/// written and read from this socket, and the kernel takes care of encryption
/// (and key updates, etc.) transparently.
///
/// Most errors return the `TlsStream<IO>`, allowing the caller to fall back
/// to software encryption with rustls.
pub fn config_ktls_server<IO>(
    mut stream: tokio_rustls::server::TlsStream<IO>,
) -> Result<KtlsStream<IO>, Error>
where
    IO: AsRawFd + AsyncRead + AsyncWrite + Unpin,
{
    let drained = drain(&mut stream);
    let (io, conn) = stream.into_inner();
    setup_inner(io.as_raw_fd(), Connection::Server(conn))?;
    Ok(KtlsStream::new(io, drained))
}

/// Configure kTLS for this socket. If this call succeeds, data can be
/// written and read from this socket, and the kernel takes care of encryption
/// (and key updates, etc.) transparently.
///
/// Most errors return the `TlsStream<IO>`, allowing the caller to fall back
/// to software encryption with rustls.
pub fn config_ktls_client<IO>(
    mut stream: tokio_rustls::client::TlsStream<IO>,
) -> Result<KtlsStream<IO>, Error>
where
    IO: AsRawFd + AsyncRead + AsyncWrite + Unpin,
{
    let drained = drain(&mut stream);
    let (io, conn) = stream.into_inner();
    setup_inner(io.as_raw_fd(), Connection::Client(conn))?;
    Ok(KtlsStream::new(io, drained))
}

/// Read all the bytes we can read without blocking. This is used to drained the
/// already-decrypted buffer from a tokio-rustls I/O type
fn drain(stream: &mut (dyn AsyncRead + Unpin)) -> Option<Vec<u8>> {
    let mut drained = vec![0u8; 16384];
    let mut rb = ReadBuf::new(&mut drained[..]);

    let noop_waker = futures::task::noop_waker();
    let mut cx = std::task::Context::from_waker(&noop_waker);

    match Pin::new(stream).poll_read(&mut cx, &mut rb) {
        std::task::Poll::Ready(_) => {
            let filled_len = rb.filled().len();
            drained.resize(filled_len, 0);
            Some(drained)
        }
        _ => None,
    }
}

fn setup_inner(fd: RawFd, conn: Connection) -> Result<(), Error> {
    let cipher_suite = match conn.negotiated_cipher_suite() {
        Some(cipher_suite) => cipher_suite,
        None => {
            return Err(Error::NoNegotiatedCipherSuite);
        }
    };

    let secrets = match conn.extract_secrets() {
        Ok(secrets) => secrets,
        Err(err) => return Err(Error::ExportSecrets(err)),
    };

    ffi::setup_ulp(fd).map_err(Error::UlpError)?;

    let tx = CryptoInfo::from_rustls(cipher_suite, secrets.tx)?;
    setup_tls_info(fd, ffi::Direction::Tx, tx)?;

    let rx = CryptoInfo::from_rustls(cipher_suite, secrets.rx)?;
    setup_tls_info(fd, ffi::Direction::Rx, rx)?;

    Ok(())
}

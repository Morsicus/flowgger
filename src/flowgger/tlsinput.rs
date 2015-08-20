
use flowgger::config::Config;
use flowgger::{Decoder, Encoder, Input};
use openssl::ssl::*;
use openssl::ssl::SslMethod::*;
use openssl::x509::X509FileType;
use std::io::{stderr, Write, BufRead, BufReader};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::str;
use std::sync::mpsc::SyncSender;
use std::thread;

const DEFAULT_CERT: &'static str = "flowgger.pem";
const DEFAULT_CIPHERS: &'static str = "ECDHE-RSA-CHACHA20-POLY1305:ECDHE-ECDSA-CHACHA20-POLY1305:ECDHE-RSA-AES128-GCM-SHA256:ECDHE-ECDSA-AES128-GCM-SHA256:ECDHE-RSA-AES256-GCM-SHA384:ECDHE-ECDSA-AES256-GCM-SHA384:DHE-RSA-AES128-GCM-SHA256:DHE-DSS-AES128-GCM-SHA256:kEDH+AESGCM:ECDHE-RSA-AES128-SHA256:ECDHE-ECDSA-AES128-SHA256:ECDHE-RSA-AES128-SHA:ECDHE-ECDSA-AES128-SHA:ECDHE-RSA-AES256-SHA384:ECDHE-ECDSA-AES256-SHA384:ECDHE-RSA-AES256-SHA:ECDHE-ECDSA-AES256-SHA:DHE-RSA-AES128-SHA256:DHE-RSA-AES128-SHA:DHE-DSS-AES128-SHA256:DHE-RSA-AES256-SHA256:DHE-DSS-AES256-SHA:DHE-RSA-AES256-SHA:AES128-GCM-SHA256:AES256-GCM-SHA384:AES128-SHA256:AES256-SHA256:AES128-SHA:AES256-SHA:AES:CAMELLIA:DES-CBC3-SHA:!aNULL:!eNULL:!EXPORT:!DES:!RC4:!MD5:!PSK:!aECDH:!EDH-DSS-DES-CBC3-SHA:!EDH-RSA-DES-CBC3-SHA:!KRB5-DES-CBC3-SH";
const DEFAULT_FRAMED: bool = true;
const DEFAULT_KEY: &'static str = "flowgger.pem";
const DEFAULT_LISTEN: &'static str = "0.0.0.0:6514";

#[derive(Clone)]
struct TlsConfig {
    cert: String,
    key: String,
    ciphers: String,
    framed: bool
}

pub struct TlsInput {
    listen: String,
    tls_config: TlsConfig
}

impl Input for TlsInput {
    fn new(config: &Config) -> TlsInput {
        let listen = config.lookup("input.listen").map_or(DEFAULT_LISTEN, |x| x.as_str().
            expect("input.listen must be an ip:port string")).to_owned();
        let cert = config.lookup("input.tls_cert").map_or(DEFAULT_CERT, |x| x.as_str().
            expect("input.tls_cert must be a path to a .pem file")).to_owned();
        let key = config.lookup("input.tls_key").map_or(DEFAULT_KEY, |x| x.as_str().
            expect("input.tls_key must be a path to a .pem file")).to_owned();
        let ciphers = config.lookup("input.tls_ciphers").map_or(DEFAULT_CIPHERS, |x| x.as_str().
            expect("input.tls_ciphers must be a string with a cipher suite")).to_owned();
        let framed = config.lookup("input.framed").map_or(DEFAULT_FRAMED, |x| x.as_bool().
            expect("input.framed must be a boolean"));

        let tls_config = TlsConfig {
            cert: cert,
            key: key,
            ciphers: ciphers,
            framed: framed
        };
        TlsInput {
            listen: listen,
            tls_config: tls_config
        }
    }

    fn accept<TE>(&self, tx: SyncSender<Vec<u8>>, decoder: Box<Decoder + Send>, encoder: TE) where TE: Encoder + Clone + Send + 'static {
        let listener = TcpListener::bind(&self.listen as &str).unwrap();
        for client in listener.incoming() {
            match client {
                Ok(client) => {
                    let tx = tx.clone();
                    let (decoder, encoder) = (decoder.clone_boxed(), encoder.clone());
                    let tls_config = self.tls_config.clone();
                    thread::spawn(move|| {
                        handle_client(client, tx, decoder, encoder, tls_config);
                    });
                }
                Err(_) => { }
            }
        }
    }
}

fn read_msglen(reader: &mut BufRead) -> Result<usize, &'static str> {
    let mut nbytes_v = Vec::with_capacity(16);
    let nbytes_vl = match reader.read_until(b' ', &mut nbytes_v) {
        Err(_) | Ok(0) | Ok(1) => return Err("EOF"),
        Ok(nbytes_vl) => nbytes_vl
    };
    let nbytes_s = match str::from_utf8(&nbytes_v[..nbytes_vl - 1]) {
        Err(_) => return Err("Invalid message length"),
        Ok(nbytes_s) => nbytes_s
    };
    let nbytes: usize = match nbytes_s.parse() {
        Err(_) => return Err("Invalid message length"),
        Ok(nbytes) => nbytes
    };
    Ok(nbytes)
}

fn handle_client<TE>(client: TcpStream, tx: SyncSender<Vec<u8>>, decoder: Box<Decoder>, encoder: TE, tls_config: TlsConfig) where TE: Encoder {
    let mut ctx = SslContext::new(Tlsv1_2).unwrap();
    ctx.set_verify(SSL_VERIFY_PEER, None); // Sigh
    ctx.set_options(SSL_OP_NO_COMPRESSION | SSL_OP_CIPHER_SERVER_PREFERENCE | SSL_OP_NO_SESSION_RESUMPTION_ON_RENEGOTIATION);
    ctx.set_certificate_file(&Path::new(&tls_config.cert), X509FileType::PEM).unwrap();
    ctx.set_private_key_file(&Path::new(&tls_config.key), X509FileType::PEM).unwrap();
    ctx.set_cipher_list(&tls_config.ciphers).unwrap();
    let sslclient = SslStream::accept(&ctx, client).unwrap();
    let mut reader = BufReader::new(sslclient);
    if tls_config.framed == false {
        for line in reader.lines() {
            let line = match line {
                Err(_) => {
                    let _ = writeln!(stderr(), "Invalid UTF-8 input");
                    continue;
                }
                Ok(line) => line
            };
            if let Err(e) = handle_line(&line, &tx, &decoder, &encoder) {
                let _ = writeln!(stderr(), "{}: [{}]", e, line.trim());
            }
        }
    } else {
        loop {
            if let Err(e) = read_msglen(&mut reader) {
                let _ = writeln!(stderr(), "{}", e);
                return;
            }
            let mut line = String::new();
            if reader.read_line(&mut line).is_err() {
                println!("err");
                return;
            }
            if let Err(e) = handle_line(&line, &tx, &decoder, &encoder) {
                let _ = writeln!(stderr(), "{}: [{}]", e, line.trim());
            }
        }
    }
}

fn handle_line<TE>(line: &String, tx: &SyncSender<Vec<u8>>, decoder: &Box<Decoder>, encoder: &TE) -> Result<(), &'static str> where TE: Encoder {
    let decoded = try!(decoder.decode(&line));
    let reencoded = try!(encoder.encode(decoded));
    tx.send(reencoded).unwrap();
    Ok(())
}

use async_std::io;
use async_std::net::UdpSocket;
use async_std::task;

// use futures::try_join;

use log::{info, warn};

use std::sync::Arc;

use trust_dns_proto::error::ProtoResult;
use trust_dns_proto::op::Message;
use trust_dns_proto::serialize::binary::{BinDecodable, BinDecoder, BinEncodable, BinEncoder};

// async fn make_http_call(uri: &str) -> Result<String, surf::Exception> {
//     info!("make_http_call uri: {}", uri);
//
//     surf::get(uri).recv_string().await
// }
//
// async fn make_http_calls() -> Result<(), surf::Exception> {
//     let uri1 = "https://httpbin.org/get";
//     let future1 = make_http_call(&uri1);
//
//     let uri2 = "https://httpbin.org/get2";
//     let future2 = make_http_call(&uri2);
//
//     info!("before try_join");
//
//     let results = try_join!(future1, future2)?;
//
//     info!("make_http_calls got results: {:#?}", results);
//
//     Ok(())
// }

fn encode_dns_message(message: &Message) -> ProtoResult<Vec<u8>> {
    let mut request_buffer = Vec::new();

    let mut encoder = BinEncoder::new(&mut request_buffer);
    match message.emit(&mut encoder) {
        Ok(()) => {
            info!(
                "encoded message request_buffer.len = {}",
                request_buffer.len()
            );
            Ok(request_buffer)
        }
        Err(e) => {
            warn!("error encoding message request buffer {}", e);
            Err(e)
        }
    }
}

fn decode_dns_message(buffer: Vec<u8>) -> ProtoResult<Message> {
    let mut decoder = BinDecoder::new(&buffer);
    match Message::read(&mut decoder) {
        Ok(message) => Ok(message),
        Err(e) => {
            warn!("error decoding dns message {}", e);
            Err(e)
        }
    }
}

enum DOHResponse {
    HTTPRequestError,
    HTTPRequestSuccess(Vec<u8>),
}

async fn make_doh_request(request_buffer: Vec<u8>) -> Result<DOHResponse, surf::Exception> {
    info!("make_doh_request");

    info!("before surf post");

    let mut response = match surf::post("https://dns.google/dns-query")
        .body_bytes(request_buffer)
        .set_header("content-type", "application/dns-message")
        .set_header("accept", "application/dns-message")
        .await
    {
        Ok(resp) => resp,
        Err(e) => {
            warn!("surf::post error {}", e);
            return Err(e);
        }
    };

    info!("after surf post response status = {}", response.status());

    if response.status() != 200 {
        return Ok(DOHResponse::HTTPRequestError);
    }

    let response_buffer = response.body_bytes().await?;
    Ok(DOHResponse::HTTPRequestSuccess(response_buffer))
}

async fn process_udp_packet(
    socket: Arc<UdpSocket>,
    buffer: Vec<u8>,
    bytes_received: usize,
    peer: std::net::SocketAddr,
) {
    info!(
        "process_udp_packet received {} from udp peer {}",
        bytes_received, peer
    );
    let mut decoder = BinDecoder::new(&buffer[0..bytes_received]);

    let mut request_message = match Message::read(&mut decoder) {
        Err(e) => {
            warn!("udp dns packet perse error {}", e);
            return;
        }
        Ok(message) => message,
    };

    // info!("parsed udp dns packet {:#?}", request_message);

    let original_id = request_message.header().id();

    request_message.set_id(0);

    if request_message.queries().len() < 1 {
        info!("request_message.queries is empty");
        return;
    }

    let request_buffer = match encode_dns_message(&request_message) {
        Err(e) => {
            warn!("encode_dns_message error {}", e);
            return;
        }
        Ok(buffer) => buffer,
    };

    let doh_response = match make_doh_request(request_buffer).await {
        Err(e) => {
            warn!("make_doh_request error {}", e);
            return;
        }
        Ok(doh_response) => doh_response,
    };

    let response_buffer = match doh_response {
        DOHResponse::HTTPRequestError => {
            warn!("got http request error");
            return;
        }
        DOHResponse::HTTPRequestSuccess(response_buffer) => response_buffer,
    };

    info!("got response_buffer length = {}", response_buffer.len());

    let mut response_message = match decode_dns_message(response_buffer) {
        Err(e) => {
            warn!("decode_dns_message error {}", e);
            return;
        }
        Ok(message) => message,
    };

    // info!("response_message = {:#?}", response_message);

    response_message.set_id(original_id);

    let response_buffer = match encode_dns_message(&response_message) {
        Err(e) => {
            warn!("encode_dns_message error {}", e);
            return;
        }
        Ok(buffer) => buffer,
    };

    match socket.send_to(&response_buffer, peer).await {
        Err(e) => {
            warn!("send_to error {}", e);
            return;
        }
        Ok(bytes_written) => {
            info!("send_to success bytes_written = {}", bytes_written);
        }
    }
}

async fn run_server() -> io::Result<()> {
    let socket = Arc::new(UdpSocket::bind("127.0.0.1:10053").await?);

    info!("Listening on {}", socket.local_addr()?);

    loop {
        let mut buf = vec![0u8; 2048];
        let (bytes_received, peer) = socket.recv_from(&mut buf).await?;

        task::spawn(process_udp_packet(
            Arc::clone(&socket),
            buf,
            bytes_received,
            peer,
        ));
    }
}

fn main() -> io::Result<()> {
    env_logger::init();

    let server_future = run_server();

    task::block_on(server_future)
}

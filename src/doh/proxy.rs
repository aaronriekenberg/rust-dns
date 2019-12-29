use crate::doh::cache::{get_cache_key, Cache, CacheKey, CacheObject};
use crate::doh::client::DOHClient;
use crate::doh::config::Configuration;
use crate::doh::localdomain::LocalDomainCache;
use crate::doh::metrics::Metrics;

use log::{debug, info, warn};

use std::convert::TryFrom;
use std::error::Error;
use std::sync::Arc;
use std::time::{Duration, Instant};

use trust_dns_proto::error::ProtoResult;
use trust_dns_proto::op::Message;
use trust_dns_proto::rr::resource::Record;
use trust_dns_proto::serialize::binary::{BinDecodable, BinDecoder, BinEncodable, BinEncoder};

pub struct DOHProxy {
    configuration: Configuration,
    local_domain_cache: LocalDomainCache,
    cache: Cache,
    doh_client: DOHClient,
    metrics: Arc<Metrics>,
}

impl DOHProxy {
    pub fn new(configuration: Configuration) -> Arc<Self> {
        let forward_domain_configurations = configuration.forward_domain_configurations().clone();
        let reverse_domain_configurations = configuration.reverse_domain_configurations().clone();
        let cache_configuration = configuration.cache_configuration().clone();
        let client_configuration = configuration.client_configuration().clone();

        Arc::new(DOHProxy {
            configuration,
            local_domain_cache: LocalDomainCache::new(
                forward_domain_configurations,
                reverse_domain_configurations,
            ),
            cache: Cache::new(cache_configuration),
            doh_client: DOHClient::new(client_configuration),
            metrics: Metrics::new(),
        })
    }

    fn encode_dns_message(&self, message: &Message) -> ProtoResult<Vec<u8>> {
        let mut request_buffer = Vec::new();

        let mut encoder = BinEncoder::new(&mut request_buffer);
        match message.emit(&mut encoder) {
            Ok(_) => {
                debug!(
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

    fn decode_dns_message_vec(&self, buffer: Vec<u8>) -> ProtoResult<Message> {
        let mut decoder = BinDecoder::new(&buffer);
        match Message::read(&mut decoder) {
            Ok(message) => Ok(message),
            Err(e) => {
                warn!("error decoding dns message {}", e);
                Err(e)
            }
        }
    }

    fn decode_dns_message_slice(&self, buffer: &[u8]) -> ProtoResult<Message> {
        let mut decoder = BinDecoder::new(&buffer);
        match Message::read(&mut decoder) {
            Ok(message) => Ok(message),
            Err(e) => {
                warn!("error decoding dns message {}", e);
                Err(e)
            }
        }
    }

    fn build_failure_response_message(&self, request: &Message) -> Message {
        let mut response_message = request.clone();
        response_message.set_message_type(trust_dns_proto::op::MessageType::Response);
        response_message.set_response_code(trust_dns_proto::op::ResponseCode::ServFail);
        response_message
    }

    fn build_failure_response_buffer(&self, request: &Message) -> Option<Vec<u8>> {
        match self.encode_dns_message(&self.build_failure_response_message(request)) {
            Err(e) => {
                warn!("build_failure_response_buffer encode error {}", e);
                None
            }
            Ok(buffer) => Some(buffer),
        }
    }

    async fn make_doh_request(&self, request_message: &Message) -> Option<Message> {
        let mut doh_request_message = request_message.clone();
        doh_request_message.set_id(0);
        let request_buffer = match self.encode_dns_message(&doh_request_message) {
            Err(e) => {
                warn!("encode_dns_message error {}", e);
                return None;
            }
            Ok(buffer) => buffer,
        };

        let doh_response = match self.doh_client.make_doh_request(request_buffer).await {
            Err(e) => {
                warn!("make_doh_request error {}", e);
                return None;
            }
            Ok(doh_response) => doh_response,
        };

        let response_buffer = match doh_response {
            crate::doh::client::DOHResponse::HTTPRequestError => {
                warn!("got http request error");
                return None;
            }
            crate::doh::client::DOHResponse::HTTPRequestSuccess(response_buffer) => response_buffer,
        };

        debug!("got response_buffer length = {}", response_buffer.len());

        let response_message = match self.decode_dns_message_vec(response_buffer) {
            Err(e) => {
                warn!("decode_dns_message error {}", e);
                return None;
            }
            Ok(message) => message,
        };

        Some(response_message)
    }

    fn clamp_and_get_min_ttl_seconds(&self, response_message: &mut Message) -> u32 {
        let clamp_min_ttl_seconds = self
            .configuration
            .proxy_configuration()
            .clamp_min_ttl_seconds();
        let clamp_max_ttl_seconds = self
            .configuration
            .proxy_configuration()
            .clamp_max_ttl_seconds();

        let mut found_record_ttl = false;
        let mut record_min_ttl_seconds = clamp_min_ttl_seconds;

        let mut process_record = |record: &mut Record| {
            let ttl = record.ttl();

            let ttl = std::cmp::max(ttl, clamp_min_ttl_seconds);
            let ttl = std::cmp::min(ttl, clamp_max_ttl_seconds);

            if (!found_record_ttl) || (ttl < record_min_ttl_seconds) {
                record_min_ttl_seconds = ttl;
                found_record_ttl = true;
            }
            record.set_ttl(ttl);
        };

        for mut record in response_message.take_answers() {
            process_record(&mut record);
            response_message.add_answer(record);
        }
        for mut record in response_message.take_name_servers() {
            process_record(&mut record);
            response_message.add_name_server(record);
        }
        for mut record in response_message.take_additionals() {
            process_record(&mut record);
            response_message.add_additional(record);
        }

        record_min_ttl_seconds
    }

    async fn clamp_ttl_and_cache_response(
        &self,
        cache_key: CacheKey,
        mut response_message: Message,
    ) -> Message {
        if !((response_message.response_code() == trust_dns_proto::op::ResponseCode::NoError)
            || (response_message.response_code() == trust_dns_proto::op::ResponseCode::NXDomain))
        {
            return response_message;
        }

        let min_ttl_seconds = self.clamp_and_get_min_ttl_seconds(&mut response_message);

        if min_ttl_seconds == 0 {
            return response_message;
        }

        if !cache_key.valid() {
            return response_message;
        }

        let now = Instant::now();
        let min_ttl_duration = Duration::from_secs(min_ttl_seconds.into());

        self.cache
            .put(
                cache_key,
                CacheObject::new(response_message.clone(), now, min_ttl_duration),
            )
            .await;

        response_message
    }

    fn get_message_for_local_domain(
        &self,
        cache_key: &CacheKey,
        request_id: u16,
    ) -> Option<Message> {
        let mut response_message = match self.local_domain_cache.get_response_message(&cache_key) {
            None => return None,
            Some(message) => message,
        };

        response_message.set_id(request_id);

        Some(response_message)
    }

    async fn get_message_for_cache_hit(
        &self,
        cache_key: &CacheKey,
        request_id: u16,
    ) -> Option<Message> {
        let mut cache_object = match self.cache.get(&cache_key).await {
            None => return None,
            Some(cache_object) => cache_object,
        };

        if cache_object.expired(Instant::now()) {
            return None;
        }

        let seconds_to_subtract_from_ttl = cache_object.duration_in_cache().as_secs();
        let mut ok = true;

        let mut adjust_record_ttl = |record: &mut Record| {
            let original_ttl = u64::from(record.ttl());
            if seconds_to_subtract_from_ttl > original_ttl {
                ok = false;
            } else {
                let new_ttl = original_ttl - seconds_to_subtract_from_ttl;
                let new_ttl = match u32::try_from(new_ttl) {
                    Ok(new_ttl) => new_ttl,
                    Err(e) => {
                        warn!(
                            "get_message_for_cache_hit new_ttl overflow {} {}",
                            new_ttl, e
                        );
                        ok = false;
                        0
                    }
                };
                record.set_ttl(new_ttl);
            }
        };

        let response_message = cache_object.message_mut();

        for mut record in response_message.take_answers() {
            adjust_record_ttl(&mut record);
            response_message.add_answer(record);
        }
        for mut record in response_message.take_name_servers() {
            adjust_record_ttl(&mut record);
            response_message.add_name_server(record);
        }
        for mut record in response_message.take_additionals() {
            adjust_record_ttl(&mut record);
            response_message.add_additional(record);
        }

        if !ok {
            return None;
        }

        response_message.set_id(request_id);

        Some(cache_object.message())
    }

    async fn process_request_message(&self, request_message: &Message) -> Message {
        debug!(
            "process_request_message request_message {:#?}",
            request_message
        );

        if request_message.queries().is_empty() {
            warn!("request_message.queries is empty");
            return self.build_failure_response_message(&request_message);
        }

        let cache_key = get_cache_key(&request_message);

        debug!("cache_key = {:#?}", cache_key);

        if let Some(response_message) =
            self.get_message_for_local_domain(&cache_key, request_message.header().id())
        {
            return response_message;
        }

        if let Some(response_message) = self
            .get_message_for_cache_hit(&cache_key, request_message.header().id())
            .await
        {
            self.metrics.increment_cache_hits();
            return response_message;
        }

        self.metrics.increment_cache_misses();

        let response_message = match self.make_doh_request(&request_message).await {
            None => return self.build_failure_response_message(&request_message),
            Some(response_message) => response_message,
        };

        let mut response_message = self
            .clamp_ttl_and_cache_response(cache_key, response_message)
            .await;
        response_message.set_id(request_message.header().id());

        response_message
    }

    pub(in crate::doh) async fn process_request_packet_buffer(
        &self,
        request_buffer: &[u8],
    ) -> Option<Vec<u8>> {
        debug!(
            "process_request_packet_buffer received {}",
            request_buffer.len()
        );

        let request_message = match self.decode_dns_message_slice(&request_buffer) {
            Err(e) => {
                warn!("decode_dns_message request error {}", e);
                return None;
            }
            Ok(message) => message,
        };

        let response_message = self.process_request_message(&request_message).await;

        match self.encode_dns_message(&response_message) {
            Err(e) => {
                warn!("encode_dns_message response error {}", e);
                self.build_failure_response_buffer(&request_message)
            }
            Ok(buffer) => Some(buffer),
        }
    }

    async fn run_periodic_timer(self: Arc<Self>) {
        info!("begin run_periodic_timer");

        let timer_duration = Duration::from_secs(self.configuration.timer_interval_seconds());

        loop {
            tokio::time::delay_for(timer_duration).await;

            let (cache_len, cache_items_purged) = self.cache.periodic_purge().await;
            info!(
                "run_periodic_timer metrics: {} cache_len = {} cache_items_purged = {}",
                self.metrics, cache_len, cache_items_purged,
            );
        }
    }

    pub async fn run(self: Arc<Self>) -> Result<(), Box<dyn Error>> {
        info!("begin run");

        tokio::spawn(Arc::clone(&self).run_periodic_timer());

        let tcp_server = crate::doh::tcpserver::TCPServer::new(
            self.configuration.server_configuration().clone(),
            Arc::clone(&self.metrics),
            Arc::clone(&self),
        );
        tokio::spawn(async move {
            if let Err(e) = tcp_server.run().await {
                warn!("run_tcp_server returned error {}", e);
            }
        });

        let udp_server = crate::doh::udpserver::UDPServer::new(
            self.configuration.server_configuration().clone(),
            Arc::clone(&self.metrics),
            Arc::clone(&self),
        );
        udp_server.run().await
    }
}

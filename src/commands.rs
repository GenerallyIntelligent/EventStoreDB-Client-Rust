//! Commands this client supports.
use futures::{stream, TryStreamExt};
use futures::{Stream, StreamExt};

use crate::event_store::client::{persistent, shared, streams};
use crate::types::{
    EventData, ExpectedRevision, PersistentSubscriptionSettings, Position, ReadDirection,
    RecordedEvent, ResolvedEvent, StreamPosition, SubEvent, WriteResult, WrongExpectedVersion,
};

use async_stream::{stream, try_stream};
use persistent::persistent_subscriptions_client::PersistentSubscriptionsClient;
use shared::{Empty, StreamIdentifier, Uuid};
use streams::streams_client::StreamsClient;

use crate::batch::BatchAppendClient;
use crate::grpc::{handle_error, GrpcClient, Msg};
use crate::options::append_to_stream::AppendToStreamOptions;
use crate::options::batch_append::BatchAppendOptions;
use crate::options::persistent_subscription::PersistentSubscriptionOptions;
use crate::options::read_all::ReadAllOptions;
use crate::options::read_stream::ReadStreamOptions;
use crate::options::subscribe_to_stream::SubscribeToStreamOptions;
use crate::{
    Credentials, CurrentRevision, DeletePersistentSubscriptionOptions, DeleteStreamOptions,
    NakAction, PersistentSubEvent, PersistentSubscriptionToAllOptions, RetryOptions,
    SubscribeToAllOptions, SubscribeToPersistentSubscriptionn, SubscriptionFilter,
    SystemConsumerStrategy, TombstoneStreamOptions,
};
use futures::stream::BoxStream;
use tonic::{Request, Streaming};

pub(crate) mod defaults {
    use std::time::Duration;

    pub const DEFAULT_WRITE_DEADLINE: Duration = Duration::from_secs(2);
}

fn raw_uuid_to_uuid(src: Uuid) -> uuid::Uuid {
    use byteorder::{BigEndian, ByteOrder};

    let value = src
        .value
        .expect("We expect Uuid value to be defined for now");

    match value {
        shared::uuid::Value::Structured(s) => {
            let mut buf = [0u8; 16];

            BigEndian::write_i64(&mut buf, s.most_significant_bits);
            BigEndian::write_i64(&mut buf[8..16], s.least_significant_bits);

            uuid::Uuid::from_bytes(buf)
        }

        shared::uuid::Value::String(s) => s
            .parse()
            .expect("We expect a valid UUID out of this String"),
    }
}

fn raw_persistent_uuid_to_uuid(src: Uuid) -> uuid::Uuid {
    use byteorder::{BigEndian, ByteOrder};

    let value = src
        .value
        .expect("We expect Uuid value to be defined for now");

    match value {
        shared::uuid::Value::Structured(s) => {
            let mut buf = [0u8; 16];

            BigEndian::write_i64(&mut buf, s.most_significant_bits);
            BigEndian::write_i64(&mut buf[8..16], s.least_significant_bits);

            uuid::Uuid::from_bytes(buf)
        }

        shared::uuid::Value::String(s) => s
            .parse()
            .expect("We expect a valid UUID out of this String"),
    }
}

fn convert_event_data(event: EventData) -> streams::AppendReq {
    use streams::append_req;

    let id = event.id_opt.unwrap_or_else(uuid::Uuid::new_v4);
    let id = shared::uuid::Value::String(id.to_string());
    let id = Uuid { value: Some(id) };
    let custom_metadata = event
        .custom_metadata
        .map_or_else(Vec::new, |b| (&*b).into());

    let msg = append_req::ProposedMessage {
        id: Some(id),
        metadata: event.metadata,
        custom_metadata,
        data: (&*event.payload).into(),
    };

    let content = append_req::Content::ProposedMessage(msg);

    streams::AppendReq {
        content: Some(content),
    }
}

fn convert_proto_recorded_event(
    event: streams::read_resp::read_event::RecordedEvent,
) -> RecordedEvent {
    let id = event
        .id
        .map(raw_uuid_to_uuid)
        .expect("Unable to parse Uuid [convert_proto_recorded_event]");

    let position = Position {
        commit: event.commit_position,
        prepare: event.prepare_position,
    };

    let event_type = if let Some(tpe) = event.metadata.get("type") {
        tpe.to_string()
    } else {
        "<no-event-type-provided>".to_string()
    };

    let is_json = if let Some(content_type) = event.metadata.get("content-type") {
        matches!(content_type.as_str(), "application/json")
    } else {
        false
    };

    let stream_id = String::from_utf8(
        event
            .stream_identifier
            .expect("stream_identifier is always defined")
            .stream_name,
    )
    .expect("It's always UTF-8");
    RecordedEvent {
        id,
        stream_id,
        revision: event.stream_revision,
        position,
        event_type,
        is_json,
        metadata: event.metadata,
        custom_metadata: event.custom_metadata.into(),
        data: event.data.into(),
    }
}

fn convert_persistent_proto_recorded_event(
    event: persistent::read_resp::read_event::RecordedEvent,
) -> RecordedEvent {
    let id = event
        .id
        .map(raw_persistent_uuid_to_uuid)
        .expect("Unable to parse Uuid [convert_persistent_proto_recorded_event]");

    let position = Position {
        commit: event.commit_position,
        prepare: event.prepare_position,
    };

    let event_type = if let Some(tpe) = event.metadata.get("type") {
        tpe.to_string()
    } else {
        "<no-event-type-provided>".to_owned()
    };

    let is_json = if let Some(content_type) = event.metadata.get("content-type") {
        matches!(content_type.as_str(), "application/json")
    } else {
        false
    };

    let stream_id = String::from_utf8(
        event
            .stream_identifier
            .expect("stream_identifier is always defined")
            .stream_name,
    )
    .expect("string is UTF-8 valid");

    RecordedEvent {
        id,
        stream_id,
        revision: event.stream_revision,
        position,
        event_type,
        is_json,
        metadata: event.metadata,
        custom_metadata: event.custom_metadata.into(),
        data: event.data.into(),
    }
}

fn convert_event_data_to_batch_proposed_message(
    event: EventData,
) -> streams::batch_append_req::ProposedMessage {
    use streams::batch_append_req;

    let id = event.id_opt.unwrap_or_else(uuid::Uuid::new_v4);
    let id = shared::uuid::Value::String(id.to_string());
    let id = Uuid { value: Some(id) };
    let custom_metadata = event
        .custom_metadata
        .map_or_else(Vec::new, |b| (&*b).into());

    batch_append_req::ProposedMessage {
        id: Some(id),
        metadata: event.metadata,
        custom_metadata,
        data: (&*event.payload).into(),
    }
}

/// This trait is added as a compatibility layer when interacting with EventStoreDB servers <= 21 version.
/// Its goal is to translate a persistent subscription starting position value to a u64 so the deprecated
/// revision field can be used.
///
/// When dealing with persistent subscription to $all, we default any persistent subscription to $all
/// starting position to 0 as the server will just ignore the field anyway.
pub(crate) trait PsPosition: Copy {
    fn to_deprecated_value(self) -> Option<u64>;
}

impl PsPosition for u64 {
    fn to_deprecated_value(self) -> Option<u64> {
        Some(self)
    }
}

impl PsPosition for Position {
    fn to_deprecated_value(self) -> Option<u64> {
        None
    }
}

fn ps_to_deprecated_revision_value<A>(value: StreamPosition<A>) -> u64
where
    A: PsPosition,
{
    match value {
        StreamPosition::Start => 0,
        StreamPosition::End => u64::MAX,
        StreamPosition::Position(value) => value.to_deprecated_value().unwrap_or(0),
    }
}

fn convert_settings_create<A>(
    settings: PersistentSubscriptionSettings<A>,
) -> persistent::create_req::Settings
where
    A: PsPosition,
{
    let named_consumer_strategy = match settings.consumer_strategy_name {
        SystemConsumerStrategy::DispatchToSingle => 0,
        SystemConsumerStrategy::RoundRobin => 1,
        SystemConsumerStrategy::Pinned => 2,
    };

    let deprecated_revision_value = ps_to_deprecated_revision_value(settings.start_from);

    #[allow(deprecated)]
    persistent::create_req::Settings {
        resolve_links: settings.resolve_link_tos,
        revision: deprecated_revision_value,
        extra_statistics: settings.extra_statistics,
        message_timeout: Some(
            persistent::create_req::settings::MessageTimeout::MessageTimeoutMs(
                settings.message_timeout.as_millis() as i32,
            ),
        ),
        max_retry_count: settings.max_retry_count,
        checkpoint_after: Some(
            persistent::create_req::settings::CheckpointAfter::CheckpointAfterMs(
                settings.checkpoint_after.as_millis() as i32,
            ),
        ),
        min_checkpoint_count: settings.checkpoint_lower_bound,
        max_checkpoint_count: settings.checkpoint_upper_bound,
        max_subscriber_count: settings.max_subscriber_count,
        live_buffer_size: settings.live_buffer_size,
        read_batch_size: settings.read_batch_size,
        history_buffer_size: settings.history_buffer_size,
        named_consumer_strategy,
    }
}

fn convert_settings_update<A>(
    settings: PersistentSubscriptionSettings<A>,
) -> persistent::update_req::Settings
where
    A: PsPosition,
{
    let named_consumer_strategy = match settings.consumer_strategy_name {
        SystemConsumerStrategy::DispatchToSingle => 0,
        SystemConsumerStrategy::RoundRobin => 1,
        SystemConsumerStrategy::Pinned => 2,
    };

    let deprecated_revision_value = ps_to_deprecated_revision_value(settings.start_from);

    #[allow(deprecated)]
    persistent::update_req::Settings {
        resolve_links: settings.resolve_link_tos,
        revision: deprecated_revision_value,
        extra_statistics: settings.extra_statistics,
        message_timeout: Some(
            persistent::update_req::settings::MessageTimeout::MessageTimeoutMs(
                settings.message_timeout.as_millis() as i32,
            ),
        ),
        max_retry_count: settings.max_retry_count,
        checkpoint_after: Some(
            persistent::update_req::settings::CheckpointAfter::CheckpointAfterMs(
                settings.checkpoint_after.as_millis() as i32,
            ),
        ),
        min_checkpoint_count: settings.checkpoint_lower_bound,
        max_checkpoint_count: settings.checkpoint_upper_bound,
        max_subscriber_count: settings.max_subscriber_count,
        live_buffer_size: settings.live_buffer_size,
        read_batch_size: settings.read_batch_size,
        history_buffer_size: settings.history_buffer_size,
        named_consumer_strategy,
    }
}

fn convert_proto_read_event(event: streams::read_resp::ReadEvent) -> ResolvedEvent {
    let commit_position = if let Some(pos_alt) = event.position {
        match pos_alt {
            streams::read_resp::read_event::Position::CommitPosition(pos) => Some(pos),
            streams::read_resp::read_event::Position::NoPosition(_) => None,
        }
    } else {
        None
    };

    ResolvedEvent {
        event: event.event.map(convert_proto_recorded_event),
        link: event.link.map(convert_proto_recorded_event),
        commit_position,
    }
}

fn convert_persistent_proto_read_event(
    event: persistent::read_resp::ReadEvent,
) -> PersistentSubEvent {
    use persistent::read_resp::read_event::Count;
    let commit_position = if let Some(pos_alt) = event.position {
        match pos_alt {
            persistent::read_resp::read_event::Position::CommitPosition(pos) => Some(pos),
            persistent::read_resp::read_event::Position::NoPosition(_) => None,
        }
    } else {
        None
    };

    let resolved = ResolvedEvent {
        event: event.event.map(convert_persistent_proto_recorded_event),
        link: event.link.map(convert_persistent_proto_recorded_event),
        commit_position,
    };

    let retry_count = event.count.map_or(0usize, |repr| match repr {
        Count::RetryCount(count) => count as usize,
        Count::NoRetryCount(_) => 0,
    });

    PersistentSubEvent {
        event: resolved,
        retry_count,
    }
}

pub(crate) fn configure_auth_req<A>(req: &mut Request<A>, creds_opt: Option<Credentials>) {
    use tonic::metadata::MetadataValue;

    if let Some(creds) = creds_opt {
        let login = String::from_utf8_lossy(&*creds.login).into_owned();
        let password = String::from_utf8_lossy(&*creds.password).into_owned();

        let basic_auth_string = base64::encode(&format!("{}:{}", login, password));
        let basic_auth = format!("Basic {}", basic_auth_string);
        let header_value = MetadataValue::from_str(basic_auth.as_str())
            .expect("Auth header value should be valid metadata header value");

        req.metadata_mut().insert("authorization", header_value);
    }
}

pub fn filter_into_proto(filter: SubscriptionFilter) -> streams::read_req::options::FilterOptions {
    use options::filter_options::{Expression, Filter, Window};
    use streams::read_req::options::{self, FilterOptions};

    let window = match filter.max {
        Some(max) => Window::Max(max),
        None => Window::Count(Empty {}),
    };

    let expr = Expression {
        regex: filter.regex.unwrap_or_else(|| "".to_string()),
        prefix: filter.prefixes,
    };

    let filter = if filter.based_on_stream {
        Filter::StreamIdentifier(expr)
    } else {
        Filter::EventType(expr)
    };

    FilterOptions {
        filter: Some(filter),
        window: Some(window),
        checkpoint_interval_multiplier: 1,
    }
}

pub fn ps_create_filter_into_proto(
    filter: &SubscriptionFilter,
) -> persistent::create_req::all_options::FilterOptions {
    use persistent::create_req::all_options::filter_options::{Expression, Filter, Window};
    use persistent::create_req::all_options::FilterOptions;

    let window = match filter.max {
        Some(max) => Window::Max(max),
        None => Window::Count(Empty {}),
    };

    let expr = Expression {
        regex: filter.regex.clone().unwrap_or_else(|| "".to_string()),
        prefix: filter.prefixes.clone(),
    };

    let filter = if filter.based_on_stream {
        Filter::StreamIdentifier(expr)
    } else {
        Filter::EventType(expr)
    };

    FilterOptions {
        filter: Some(filter),
        window: Some(window),
        checkpoint_interval_multiplier: 1,
    }
}

/// Sends asynchronously the write command to the server.
pub async fn append_to_stream<S, Events>(
    connection: &GrpcClient,
    stream: S,
    options: &AppendToStreamOptions,
    events: Events,
) -> crate::Result<Result<WriteResult, WrongExpectedVersion>>
where
    S: AsRef<str>,
    Events: Stream<Item = EventData> + Send + Sync + 'static,
{
    use streams::append_req::{self, Content};
    use streams::AppendReq;

    let stream = stream.as_ref().to_string();

    connection.execute(move |channel| async move {
        let stream_identifier = Some(StreamIdentifier {
            stream_name: stream.into_bytes(),
        });
        let header = Content::Options(append_req::Options {
            stream_identifier,
            expected_stream_revision: Some(options.version.clone()),
        });
        let header = AppendReq {
            content: Some(header),
        };
        let header = stream::once(async move { header });
        let events = events.map(convert_event_data);
        let payload = header.chain(events);
        let mut req = Request::new(payload);

        let credentials = options.credentials.clone().or_else(|| connection.default_credentials());

        configure_auth_req(&mut req, credentials);

        let mut client = StreamsClient::new(channel.channel);
        let resp = client.append(req).await?.into_inner();

        match resp.result.unwrap() {
            streams::append_resp::Result::Success(success) => {
                let next_expected_version = match success.current_revision_option.unwrap() {
                    streams::append_resp::success::CurrentRevisionOption::CurrentRevision(rev) => {
                        rev
                    }
                    streams::append_resp::success::CurrentRevisionOption::NoStream(_) => 0,
                };

                let position = match success.position_option.unwrap() {
                    streams::append_resp::success::PositionOption::Position(pos) => Position {
                        commit: pos.commit_position,
                        prepare: pos.prepare_position,
                    },

                    streams::append_resp::success::PositionOption::NoPosition(_) => {
                        Position::start()
                    }
                };

                let write_result = WriteResult {
                    next_expected_version,
                    position,
                };

                Ok(Ok(write_result))
            }

            streams::append_resp::Result::WrongExpectedVersion(error) => {
                let current = match error.current_revision_option.unwrap() {
                    streams::append_resp::wrong_expected_version::CurrentRevisionOption::CurrentRevision(rev) => CurrentRevision::Current(rev),
                    streams::append_resp::wrong_expected_version::CurrentRevisionOption::CurrentNoStream(_) => CurrentRevision::NoStream,
                };

                let expected = match error.expected_revision_option.unwrap() {
                    streams::append_resp::wrong_expected_version::ExpectedRevisionOption::ExpectedRevision(rev) => ExpectedRevision::Exact(rev),
                    streams::append_resp::wrong_expected_version::ExpectedRevisionOption::ExpectedAny(_) => ExpectedRevision::Any,
                    streams::append_resp::wrong_expected_version::ExpectedRevisionOption::ExpectedStreamExists(_) => ExpectedRevision::StreamExists,
                    streams::append_resp::wrong_expected_version::ExpectedRevisionOption::ExpectedNoStream(_) => ExpectedRevision::NoStream,
                };

                Ok(Err(WrongExpectedVersion { current, expected }))
            }
        }
    }).await
}

pub fn batch_append(connection: &GrpcClient, options: &BatchAppendOptions) -> BatchAppendClient {
    use futures::SinkExt;
    use streams::{
        batch_append_req::{options::ExpectedStreamPosition, Options, ProposedMessage},
        batch_append_resp::{
            self,
            success::{CurrentRevisionOption, PositionOption},
        },
        BatchAppendReq,
    };

    let connection = connection.clone();
    let (forward, receiver) = futures::channel::mpsc::unbounded::<crate::batch::Req>();
    let (batch_sender, batch_receiver) = futures::channel::mpsc::unbounded();
    let mut cloned_batch_sender = batch_sender.clone();
    let batch_client = BatchAppendClient::new(batch_sender, batch_receiver, forward);

    let credentials = options
        .credentials
        .as_ref()
        .cloned()
        .or_else(|| connection.default_credentials());

    let receiver = receiver.map(|req| {
        let correlation_id = shared::uuid::Value::String(req.id.to_string());
        let correlation_id = Some(Uuid {
            value: Some(correlation_id),
        });
        let stream_identifier = Some(StreamIdentifier {
            stream_name: req.stream_name.into_bytes(),
        });

        let expected_stream_position = match req.expected_revision {
            ExpectedRevision::Exact(rev) => ExpectedStreamPosition::StreamPosition(rev),
            ExpectedRevision::NoStream => ExpectedStreamPosition::NoStream(()),
            ExpectedRevision::StreamExists => ExpectedStreamPosition::StreamExists(()),
            ExpectedRevision::Any => ExpectedStreamPosition::Any(()),
        };

        let expected_stream_position = Some(expected_stream_position);

        let proposed_messages: Vec<ProposedMessage> = req
            .events
            .into_iter()
            .map(convert_event_data_to_batch_proposed_message)
            .collect();

        let options = Some(Options {
            stream_identifier,
            deadline: None,
            expected_stream_position,
        });

        BatchAppendReq {
            correlation_id,
            options,
            proposed_messages,
            is_final: true,
        }
    });

    tokio::spawn(async move {
        let result = connection
            .execute(move |handle| async move {
                let mut req = Request::new(receiver);
                configure_auth_req(&mut req, credentials);
                let mut client = StreamsClient::new(handle.channel.clone());

                let resp = client.batch_append(req).await?;

                Ok((handle, resp.into_inner()))
            })
            .await;

        match result {
            Err(e) => {
                let _ = cloned_batch_sender
                    .send(crate::batch::BatchMsg::Error(e))
                    .await;
            }

            Ok((handle, resp_stream)) => {
                let mut resp_stream = resp_stream.map_ok(|resp| {
                    let stream_name =
                        String::from_utf8(resp.stream_identifier.unwrap().stream_name)
                            .expect("valid UTF-8 string");

                    let correlation_id = raw_uuid_to_uuid(resp.correlation_id.unwrap());
                    let result = match resp.result.unwrap() {
                        batch_append_resp::Result::Success(success) => {
                            let current_revision =
                                success.current_revision_option.and_then(|rev| match rev {
                                    CurrentRevisionOption::CurrentRevision(rev) => Some(rev),
                                    CurrentRevisionOption::NoStream(()) => None,
                                });

                            let position = success.position_option.and_then(|pos| match pos {
                                PositionOption::Position(pos) => Some(Position {
                                    commit: pos.commit_position,
                                    prepare: pos.prepare_position,
                                }),
                                PositionOption::NoPosition(_) => None,
                            });

                            let expected_version =
                                resp.expected_stream_position.map(|exp| match exp {
                                    batch_append_resp::ExpectedStreamPosition::Any(_) => {
                                        crate::types::ExpectedRevision::Any
                                    }
                                    batch_append_resp::ExpectedStreamPosition::NoStream(_) => {
                                        crate::types::ExpectedRevision::NoStream
                                    }
                                    batch_append_resp::ExpectedStreamPosition::StreamExists(_) => {
                                        crate::types::ExpectedRevision::StreamExists
                                    }
                                    batch_append_resp::ExpectedStreamPosition::StreamPosition(
                                        rev,
                                    ) => crate::types::ExpectedRevision::Exact(rev),
                                });

                            Ok(crate::batch::BatchWriteResult::new(
                                stream_name,
                                current_revision,
                                position,
                                expected_version,
                            ))
                        }
                        batch_append_resp::Result::Error(code) => {
                            let message = code.message;
                            let code = tonic::Code::from(code.code);
                            let err = crate::Error::Grpc { code, message };

                            Err(err)
                        }
                    };

                    crate::batch::Out {
                        correlation_id,
                        result,
                    }
                });

                tokio::spawn(async move {
                    loop {
                        match resp_stream.try_next().await {
                            Err(e) => {
                                let err = crate::Error::from_grpc(e);
                                crate::grpc::handle_error(handle.sender(), handle.id(), &err).await;

                                // We notify the batch-append client that its session has been closed because of a gRPC error.
                                let _ = cloned_batch_sender
                                    .send(crate::batch::BatchMsg::Error(err))
                                    .await;
                                break;
                            }

                            Ok(out) => {
                                if let Some(out) = out {
                                    if cloned_batch_sender
                                        .send(crate::batch::BatchMsg::Out(out))
                                        .await
                                        .is_err()
                                    {
                                        break;
                                    }

                                    continue;
                                }

                                break;
                            }
                        }
                    }
                });
            }
        }

        Ok::<(), crate::Error>(())
    });

    batch_client
}

#[derive(Debug)]
pub struct ReadStream {
    sender: futures::channel::mpsc::UnboundedSender<Msg>,
    channel_id: uuid::Uuid,
    inner: Streaming<crate::event_store::client::streams::ReadResp>,
}

impl ReadStream {
    pub async fn next_event(&mut self) -> crate::Result<Option<ResolvedEvent>> {
        loop {
            match self.inner.try_next().await.map_err(crate::Error::from_grpc) {
                Err(e) => {
                    handle_error(&self.sender, self.channel_id, &e).await;
                    return Err(e);
                }

                Ok(resp) => {
                    if let Some(resp) = resp {
                        match resp.content.unwrap() {
                            streams::read_resp::Content::StreamNotFound(_) => {
                                return Err(crate::Error::ResourceNotFound)
                            }
                            streams::read_resp::Content::Event(event) => {
                                return Ok(Some(convert_proto_read_event(event)))
                            }
                            _ => continue,
                        }
                    }

                    return Ok(None);
                }
            }
        }
    }

    pub fn into_stream(mut self) -> impl Stream<Item = crate::Result<ResolvedEvent>> {
        try_stream! {
            while let Some(event) = self.next_event().await? {
                yield event;
            }
        }
    }
}

/// Sends asynchronously the read command to the server.
pub async fn read_stream<S: AsRef<str>>(
    connection: GrpcClient,
    options: &ReadStreamOptions,
    stream: S,
    count: u64,
) -> crate::Result<ReadStream> {
    use streams::read_req::options::stream_options::RevisionOption;
    use streams::read_req::options::{self, StreamOption, StreamOptions};
    use streams::read_req::Options;

    let read_direction = match options.direction {
        ReadDirection::Forward => 0,
        ReadDirection::Backward => 1,
    };

    let revision_option = match options.position {
        StreamPosition::Position(rev) => RevisionOption::Revision(rev),
        StreamPosition::Start => RevisionOption::Start(Empty {}),
        StreamPosition::End => RevisionOption::End(Empty {}),
    };

    let stream_identifier = Some(StreamIdentifier {
        stream_name: stream.as_ref().to_string().into_bytes(),
    });
    let stream_options = StreamOptions {
        stream_identifier,
        revision_option: Some(revision_option),
    };

    let uuid_option = options::UuidOption {
        content: Some(options::uuid_option::Content::String(Empty {})),
    };

    let credentials = options
        .credentials
        .clone()
        .or_else(|| connection.default_credentials());

    let options = Options {
        stream_option: Some(StreamOption::Stream(stream_options)),
        resolve_links: options.resolve_link_tos,
        filter_option: Some(options::FilterOption::NoFilter(Empty {})),
        count_option: Some(options::CountOption::Count(count)),
        uuid_option: Some(uuid_option),
        control_option: None,
        read_direction,
    };

    let req = streams::ReadReq {
        options: Some(options),
    };

    let mut req = Request::new(req);

    configure_auth_req(&mut req, credentials);

    let handle = connection.current_selected_node().await?;
    let channel_id = handle.id();
    let mut client = StreamsClient::new(handle.channel);

    match client.read(req).await {
        Err(status) => {
            let e = crate::Error::from_grpc(status);
            handle_error(&connection.sender, channel_id, &e).await;

            Err(e)
        }

        Ok(resp) => Ok(ReadStream {
            sender: connection.sender.clone(),
            channel_id,
            inner: resp.into_inner(),
        }),
    }
}

pub async fn read_all(
    connection: GrpcClient,
    options: &ReadAllOptions,
    count: u64,
) -> crate::Result<ReadStream> {
    use streams::read_req::options::all_options::AllOption;
    use streams::read_req::options::{self, AllOptions, StreamOption};
    use streams::read_req::Options;

    let read_direction = match options.direction {
        ReadDirection::Forward => 0,
        ReadDirection::Backward => 1,
    };

    let all_option = match options.position {
        StreamPosition::Position(pos) => {
            let pos = options::Position {
                commit_position: pos.commit,
                prepare_position: pos.prepare,
            };

            AllOption::Position(pos)
        }

        StreamPosition::Start => AllOption::Start(Empty {}),
        StreamPosition::End => AllOption::End(Empty {}),
    };

    let stream_options = AllOptions {
        all_option: Some(all_option),
    };

    let uuid_option = options::UuidOption {
        content: Some(options::uuid_option::Content::String(Empty {})),
    };

    let credentials = options
        .credentials
        .clone()
        .or_else(|| connection.default_credentials());

    let options = Options {
        stream_option: Some(StreamOption::All(stream_options)),
        resolve_links: options.resolve_link_tos,
        filter_option: Some(options::FilterOption::NoFilter(Empty {})),
        count_option: Some(options::CountOption::Count(count)),
        uuid_option: Some(uuid_option),
        control_option: None,
        read_direction,
    };

    let req = streams::ReadReq {
        options: Some(options),
    };

    let mut req = Request::new(req);

    configure_auth_req(&mut req, credentials);

    let handle = connection.current_selected_node().await?;
    let channel_id = handle.id();
    let mut client = StreamsClient::new(handle.channel);

    match client.read(req).await {
        Err(status) => {
            let e = crate::Error::from_grpc(status);
            handle_error(&connection.sender, channel_id, &e).await;

            Err(e)
        }

        Ok(resp) => Ok(ReadStream {
            sender: connection.sender.clone(),
            channel_id,
            inner: resp.into_inner(),
        }),
    }
}

/// Sends asynchronously the delete command to the server.
pub async fn delete_stream<S: AsRef<str>>(
    connection: &GrpcClient,
    stream: S,
    options: &DeleteStreamOptions,
) -> crate::Result<Option<Position>> {
    let credentials = options
        .credentials
        .clone()
        .or_else(|| connection.default_credentials());

    use streams::delete_req::options::ExpectedStreamRevision;
    use streams::delete_req::Options;
    use streams::delete_resp::PositionOption;

    let expected_stream_revision = match options.version {
        ExpectedRevision::Any => ExpectedStreamRevision::Any(Empty {}),
        ExpectedRevision::NoStream => ExpectedStreamRevision::NoStream(Empty {}),
        ExpectedRevision::StreamExists => ExpectedStreamRevision::StreamExists(Empty {}),
        ExpectedRevision::Exact(rev) => ExpectedStreamRevision::Revision(rev),
    };

    let expected_stream_revision = Some(expected_stream_revision);
    let stream_identifier = Some(StreamIdentifier {
        stream_name: stream.as_ref().to_string().into_bytes(),
    });
    let options = Options {
        stream_identifier,
        expected_stream_revision,
    };

    let mut req = Request::new(streams::DeleteReq {
        options: Some(options),
    });

    configure_auth_req(&mut req, credentials);

    connection
        .execute(|channel| async {
            let mut client = StreamsClient::new(channel.channel);
            let result = client.delete(req).await?.into_inner();

            if let Some(opts) = result.position_option {
                match opts {
                    PositionOption::Position(pos) => {
                        let pos = Position {
                            commit: pos.commit_position,
                            prepare: pos.prepare_position,
                        };

                        Ok(Some(pos))
                    }

                    PositionOption::NoPosition(_) => Ok(None),
                }
            } else {
                Ok(None)
            }
        })
        .await
}

/// Sends asynchronously the tombstone command to the server.
pub async fn tombstone_stream<S: AsRef<str>>(
    connection: &GrpcClient,
    stream: S,
    options: &TombstoneStreamOptions,
) -> crate::Result<Option<Position>> {
    let credentials = options
        .credentials
        .clone()
        .or_else(|| connection.default_credentials());

    use streams::tombstone_req::options::ExpectedStreamRevision;
    use streams::tombstone_req::Options;
    use streams::tombstone_resp::PositionOption;

    let expected_stream_revision = match options.version {
        ExpectedRevision::Any => ExpectedStreamRevision::Any(Empty {}),
        ExpectedRevision::NoStream => ExpectedStreamRevision::NoStream(Empty {}),
        ExpectedRevision::StreamExists => ExpectedStreamRevision::StreamExists(Empty {}),
        ExpectedRevision::Exact(rev) => ExpectedStreamRevision::Revision(rev),
    };

    let expected_stream_revision = Some(expected_stream_revision);
    let stream_identifier = Some(StreamIdentifier {
        stream_name: stream.as_ref().to_string().into_bytes(),
    });
    let options = Options {
        stream_identifier,
        expected_stream_revision,
    };

    let mut req = Request::new(streams::TombstoneReq {
        options: Some(options),
    });

    configure_auth_req(&mut req, credentials);

    connection
        .execute(|channel| async {
            let mut client = StreamsClient::new(channel.channel);
            let result = client.tombstone(req).await?.into_inner();

            if let Some(opts) = result.position_option {
                match opts {
                    PositionOption::Position(pos) => {
                        let pos = Position {
                            commit: pos.commit_position,
                            prepare: pos.prepare_position,
                        };

                        Ok(Some(pos))
                    }

                    PositionOption::NoPosition(_) => Ok(None),
                }
            } else {
                Ok(None)
            }
        })
        .await
}

fn retryable_subscription<'a>(
    connection: GrpcClient,
    retry: Option<RetryOptions>,
    credentials: Option<Credentials>,
    mut options: streams::read_req::Options,
) -> BoxStream<'a, crate::Result<SubEvent<ResolvedEvent>>> {
    use streams::read_req::options::all_options::AllOption;
    use streams::read_req::options::stream_options::RevisionOption;
    use streams::read_req::options::{self, StreamOption};

    let (limit, delay, retry_enabled) = if let Some(retry) = retry {
        (retry.limit, retry.delay, true)
    } else {
        (1, Default::default(), false)
    };

    let stream = try_stream! {
        let mut attempts = 1;
        loop {
            let mut req = Request::new(streams::ReadReq {
                options: Some(options.clone()),
            });

            configure_auth_req(&mut req, credentials.as_ref().cloned());

            let result = connection.execute(|channel| async move {
                let id = channel.id();
                let mut client = StreamsClient::new(channel.channel);
                let stream = client.read(req).await?.into_inner();

                Ok((id, stream))
            }).await;

            match result {
                Err(e) => {
                    if attempts < limit {
                        error!("Subscription: attempt ({}/{}) failure, cause: {}, retrying...", attempts, limit, e);
                        attempts += 1;
                        tokio::time::sleep(delay).await;
                        continue;
                    }

                    if retry_enabled {
                        error!("Subscription: maximum retry threshold reached, cause: {}", e);
                    }

                    Err(e)?;
                }

                Ok((id, mut stream)) => {
                    let mut failed = false;
                    while !failed {
                        match stream.try_next().await {
                            Err(status) => {
                                let e = crate::Error::from_grpc(status);

                                handle_error(&connection.sender, id, &e).await;

                                failed = true;
                                attempts = 1;

                                error!("Subscription dropped. cause: {}", e);
                                if !retry_enabled {
                                    Err(e)?;
                                }
                            }

                            Ok(resp) => {
                                if let Some(msg) = resp.and_then(|r| r.content) {
                                    match msg {
                                        streams::read_resp::Content::Event(event) => {
                                            let event = convert_proto_read_event(event);
                                            let stream_options = options
                                                .stream_option
                                                .as_mut()
                                                .unwrap();

                                            match stream_options {
                                                StreamOption::Stream(stream_options) => {
                                                    let revision = RevisionOption::Revision(event.get_original_event().revision as u64);
                                                    stream_options.revision_option = Some(revision);

                                                    yield SubEvent::EventAppeared(event);
                                                }

                                                StreamOption::All(all_options) => {
                                                    let position = event.get_original_event().position;
                                                    let position = options::Position {
                                                        prepare_position: position.prepare,
                                                        commit_position: position.commit,
                                                    };

                                                    all_options.all_option = Some(AllOption::Position(position));
                                                }
                                            }
                                        }

                                        streams::read_resp::Content::Confirmation(info) => {
                                            yield SubEvent::Confirmed(info.subscription_id);
                                        }

                                        _ => {}
                                    }
                                    continue;
                                }

                                error!("Unexpected behavior, the subscription ended like it was a regular read operation!");
                                unreachable!();
                            }
                        }
                    }

                    attempts += 1;
                }
            }
        }
    };

    Box::pin(stream)
}

#[derive(Debug)]
pub struct Subscription {
    sender: futures::channel::mpsc::UnboundedSender<Msg>,
    channel_id: uuid::Uuid,
    inner: Streaming<crate::event_store::client::streams::ReadResp>,
}

impl Subscription {
    pub async fn next_event(&mut self) -> crate::Result<ResolvedEvent> {
        todo!()
    }
}

/// Runs the subscription command.
pub fn subscribe_to_stream<'a, S: AsRef<str>>(
    connection: GrpcClient,
    stream_id: S,
    options: &SubscribeToStreamOptions,
) -> BoxStream<'a, crate::Result<SubEvent<ResolvedEvent>>> {
    use streams::read_req::options::stream_options::RevisionOption;
    use streams::read_req::options::{self, StreamOption, StreamOptions, SubscriptionOptions};
    use streams::read_req::Options;

    let retry = options.retry.as_ref().cloned();
    let read_direction = 0; // <- Going forward.

    let revision = match options.position {
        StreamPosition::Start => RevisionOption::Start(Empty {}),
        StreamPosition::End => RevisionOption::End(Empty {}),
        StreamPosition::Position(revision) => RevisionOption::Revision(revision),
    };

    let stream_identifier = Some(StreamIdentifier {
        stream_name: stream_id.as_ref().to_string().into_bytes(),
    });
    let stream_options = StreamOptions {
        stream_identifier,
        revision_option: Some(revision),
    };

    let uuid_option = options::UuidOption {
        content: Some(options::uuid_option::Content::String(Empty {})),
    };

    let credentials = options
        .credentials
        .clone()
        .or_else(|| connection.default_credentials());

    let options = Options {
        stream_option: Some(StreamOption::Stream(stream_options)),
        resolve_links: options.resolve_link_tos,
        filter_option: Some(options::FilterOption::NoFilter(Empty {})),
        count_option: Some(options::CountOption::Subscription(SubscriptionOptions {})),
        uuid_option: Some(uuid_option),
        control_option: None,
        read_direction,
    };

    retryable_subscription(connection, retry, credentials, options)
}

pub fn subscribe_to_all<'a>(
    connection: GrpcClient,
    options: &SubscribeToAllOptions,
) -> BoxStream<'a, crate::Result<SubEvent<ResolvedEvent>>> {
    use streams::read_req::options::all_options::AllOption;
    use streams::read_req::options::{self, AllOptions, StreamOption, SubscriptionOptions};
    use streams::read_req::Options;

    let retry = options.retry.as_ref().cloned();
    let read_direction = 0; // <- Going forward.

    let revision = match options.position {
        StreamPosition::Start => AllOption::Start(Empty {}),
        StreamPosition::Position(pos) => AllOption::Position(options::Position {
            prepare_position: pos.prepare,
            commit_position: pos.commit,
        }),
        StreamPosition::End => AllOption::End(Empty {}),
    };

    let stream_options = AllOptions {
        all_option: Some(revision),
    };

    let uuid_option = options::UuidOption {
        content: Some(options::uuid_option::Content::String(Empty {})),
    };

    let filter_option = match options.filter.as_ref() {
        Some(filter) => options::FilterOption::Filter(filter_into_proto(filter.clone())),
        None => options::FilterOption::NoFilter(Empty {}),
    };

    let credentials = options
        .credentials
        .clone()
        .or_else(|| connection.default_credentials());

    let options = Options {
        stream_option: Some(StreamOption::All(stream_options)),
        resolve_links: options.resolve_link_tos,
        filter_option: Some(filter_option),
        count_option: Some(options::CountOption::Subscription(SubscriptionOptions {})),
        uuid_option: Some(uuid_option),
        control_option: None,
        read_direction,
    };

    retryable_subscription(connection, retry, credentials, options)
}

/// This trait is used to avoid code duplication when introducing persistent subscription to $all. It
/// allows us to re-use most of regular persistent subscription code.
pub(crate) trait PsSettings {
    type Pos: PsPosition;

    fn settings(&self) -> PersistentSubscriptionSettings<Self::Pos>;

    fn credentials(&self) -> Option<Credentials>;

    fn to_create_options(
        &self,
        stream_identifier: StreamIdentifier,
    ) -> persistent::create_req::options::StreamOption;

    fn to_update_options(
        &self,
        stream_identifier: StreamIdentifier,
    ) -> persistent::update_req::options::StreamOption;
}

impl PsSettings for PersistentSubscriptionOptions {
    type Pos = u64;

    fn settings(&self) -> PersistentSubscriptionSettings<Self::Pos> {
        self.setts
    }

    fn credentials(&self) -> Option<Credentials> {
        self.credentials.clone()
    }

    fn to_create_options(
        &self,
        stream_identifier: StreamIdentifier,
    ) -> persistent::create_req::options::StreamOption {
        use persistent::create_req::{
            options::StreamOption, stream_options::RevisionOption, StreamOptions,
        };

        let revision_option = match self.setts.start_from {
            StreamPosition::Start => RevisionOption::Start(Empty {}),
            StreamPosition::End => RevisionOption::End(Empty {}),
            StreamPosition::Position(rev) => RevisionOption::Revision(rev),
        };

        StreamOption::Stream(StreamOptions {
            stream_identifier: Some(stream_identifier),
            revision_option: Some(revision_option),
        })
    }

    fn to_update_options(
        &self,
        stream_identifier: StreamIdentifier,
    ) -> persistent::update_req::options::StreamOption {
        use persistent::update_req::{
            options::StreamOption, stream_options::RevisionOption, StreamOptions,
        };

        let revision_option = match self.setts.start_from {
            StreamPosition::Start => RevisionOption::Start(Empty {}),
            StreamPosition::End => RevisionOption::End(Empty {}),
            StreamPosition::Position(rev) => RevisionOption::Revision(rev),
        };

        StreamOption::Stream(StreamOptions {
            stream_identifier: Some(stream_identifier),
            revision_option: Some(revision_option),
        })
    }
}

impl PsSettings for PersistentSubscriptionToAllOptions {
    type Pos = Position;

    fn settings(&self) -> PersistentSubscriptionSettings<Self::Pos> {
        self.setts
    }

    fn credentials(&self) -> Option<Credentials> {
        self.credentials.clone()
    }

    fn to_create_options(
        &self,
        _stream_identifier: StreamIdentifier,
    ) -> persistent::create_req::options::StreamOption {
        use persistent::create_req::{
            self,
            all_options::{self, AllOption},
            options::StreamOption,
            AllOptions,
        };

        let filter_option = match self.filter.as_ref() {
            Some(filter) => all_options::FilterOption::Filter(ps_create_filter_into_proto(filter)),
            None => all_options::FilterOption::NoFilter(Empty {}),
        };

        let all_option = match self.setts.start_from {
            StreamPosition::Start => AllOption::Start(Empty {}),
            StreamPosition::End => AllOption::End(Empty {}),
            StreamPosition::Position(pos) => AllOption::Position(create_req::Position {
                commit_position: pos.commit,
                prepare_position: pos.prepare,
            }),
        };

        StreamOption::All(AllOptions {
            filter_option: Some(filter_option),
            all_option: Some(all_option),
        })
    }

    fn to_update_options(
        &self,
        _stream_identifier: StreamIdentifier,
    ) -> persistent::update_req::options::StreamOption {
        use persistent::update_req::{
            self, all_options::AllOption, options::StreamOption, AllOptions,
        };

        let all_option = match self.setts.start_from {
            StreamPosition::Start => AllOption::Start(Empty {}),
            StreamPosition::End => AllOption::End(Empty {}),
            StreamPosition::Position(pos) => AllOption::Position(update_req::Position {
                commit_position: pos.commit,
                prepare_position: pos.prepare,
            }),
        };

        StreamOption::All(AllOptions {
            all_option: Some(all_option),
        })
    }
}

pub(crate) async fn create_persistent_subscription<S: AsRef<str>, Options>(
    connection: &GrpcClient,
    stream: S,
    group: S,
    options: &Options,
) -> crate::Result<()>
where
    Options: PsSettings,
{
    use persistent::create_req::Options;
    use persistent::CreateReq;

    let settings = convert_settings_create(options.settings());
    let stream_identifier = StreamIdentifier {
        stream_name: stream.as_ref().to_string().into_bytes(),
    };

    let credentials = options
        .credentials()
        .or_else(|| connection.default_credentials());

    #[allow(deprecated)]
    let options = Options {
        stream_option: Some(options.to_create_options(stream_identifier.clone())),
        stream_identifier: Some(stream_identifier),
        group_name: group.as_ref().to_string(),
        settings: Some(settings),
    };

    let req = CreateReq {
        options: Some(options),
    };

    let mut req = Request::new(req);

    configure_auth_req(&mut req, credentials);
    req.set_timeout(defaults::DEFAULT_WRITE_DEADLINE);

    connection
        .execute(|channel| async {
            let mut client = PersistentSubscriptionsClient::new(channel.channel);
            client.create(req).await?;

            Ok(())
        })
        .await
}

pub(crate) async fn update_persistent_subscription<S: AsRef<str>, Options>(
    connection: &GrpcClient,
    stream: S,
    group: S,
    options: &Options,
) -> crate::Result<()>
where
    Options: PsSettings,
{
    use persistent::update_req::Options;
    use persistent::UpdateReq;

    let settings = convert_settings_update(options.settings());
    let stream_identifier = StreamIdentifier {
        stream_name: stream.as_ref().to_string().into_bytes(),
    };

    let credentials = options
        .credentials()
        .or_else(|| connection.default_credentials());

    #[allow(deprecated)]
    let options = Options {
        group_name: group.as_ref().to_string(),
        stream_option: Some(options.to_update_options(stream_identifier.clone())),
        stream_identifier: Some(stream_identifier),
        settings: Some(settings),
    };

    let req = UpdateReq {
        options: Some(options),
    };

    let mut req = Request::new(req);

    configure_auth_req(&mut req, credentials);
    req.set_timeout(defaults::DEFAULT_WRITE_DEADLINE);

    connection
        .execute(|channel| async {
            let mut client = PersistentSubscriptionsClient::new(channel.channel);
            client.update(req).await?;

            Ok(())
        })
        .await
}

pub async fn delete_persistent_subscription<S: AsRef<str>>(
    connection: &GrpcClient,
    stream_id: S,
    group_name: S,
    options: &DeletePersistentSubscriptionOptions,
    to_all: bool,
) -> crate::Result<()> {
    use persistent::delete_req::{options::StreamOption, Options};

    let stream_option = if !to_all {
        StreamOption::StreamIdentifier(StreamIdentifier {
            stream_name: stream_id.as_ref().to_string().into_bytes(),
        })
    } else {
        StreamOption::All(Empty {})
    };

    let stream_option = Some(stream_option);

    let credentials = options
        .credentials
        .clone()
        .or_else(|| connection.default_credentials());

    let options = Options {
        stream_option,
        group_name: group_name.as_ref().to_string(),
    };

    let req = persistent::DeleteReq {
        options: Some(options),
    };

    let mut req = Request::new(req);

    configure_auth_req(&mut req, credentials);
    req.set_timeout(defaults::DEFAULT_WRITE_DEADLINE);

    connection
        .execute(|channel| async {
            let mut client = PersistentSubscriptionsClient::new(channel.channel);
            client.delete(req).await?;

            Ok(())
        })
        .await
}

/// Sends the persistent subscription connection request to the server
/// asynchronously even if the subscription is available right away.
pub async fn subscribe_to_persistent_subscription<S: AsRef<str>>(
    connection: &GrpcClient,
    stream_id: S,
    group_name: S,
    options: &SubscribeToPersistentSubscriptionn,
    to_all: bool,
) -> crate::Result<(SubscriptionRead, SubscriptionWrite)> {
    use futures::channel::mpsc;
    use futures::sink::SinkExt;
    use persistent::read_req::options::{self, UuidOption};
    use persistent::read_req::{self, options::StreamOption, Options};
    use persistent::read_resp;
    use persistent::ReadReq;

    let (mut sender, recv) = mpsc::channel(500);

    let uuid_option = UuidOption {
        content: Some(options::uuid_option::Content::String(Empty {})),
    };

    let stream_option = if !to_all {
        StreamOption::StreamIdentifier(StreamIdentifier {
            stream_name: stream_id.as_ref().to_string().into_bytes(),
        })
    } else {
        StreamOption::All(Empty {})
    };

    let stream_option = Some(stream_option);

    let credentials = options
        .credentials
        .clone()
        .or_else(|| connection.default_credentials());

    let options = Options {
        stream_option,
        group_name: group_name.as_ref().to_string(),
        buffer_size: options.buffer_size as i32,
        uuid_option: Some(uuid_option),
    };

    let read_req = ReadReq {
        content: Some(read_req::Content::Options(options)),
    };

    let mut req = Request::new(recv);

    configure_auth_req(&mut req, credentials);

    let _ = sender.send(read_req).await;
    let stream_id = stream_id.as_ref().to_string();
    let group_name = group_name.as_ref().to_string();
    connection
        .execute(|channel| async move {
            let mut client = PersistentSubscriptionsClient::new(channel.channel.clone());
            let mut stream = client.read(req).await?.into_inner();

            let stream = stream! {
                loop {
                    match stream.try_next().await {
                        Err(e) => {
                            if let Some("persistent-subscription-dropped") = e
                                .metadata()
                                .get("exception").and_then(|e| e.to_str().ok())
                            {
                                warn!(
                                    "Persistent subscription on '{}' and group '{}' has dropped",
                                    stream_id,
                                    group_name,
                                );

                                break;
                            }

                            let e = crate::Error::from_grpc(e);
                            channel.report_error(&e).await;
                            yield Err(e);
                            break;
                        }

                        Ok(resp) => {
                            if let Some(resp) = resp {
                                match resp.content.expect("content is defined") {
                                    read_resp::Content::Event(event) => {
                                        yield Ok(SubEvent::EventAppeared(convert_persistent_proto_read_event(event)));
                                    }

                                    read_resp::Content::SubscriptionConfirmation(sub) => {
                                        yield Ok(SubEvent::Confirmed(sub.subscription_id));
                                    }
                                }
                                continue;
                            }

                            break;
                        }
                    }
                }
            };

            let read = SubscriptionRead {
                inner: Box::pin(stream),
            };
            let write = SubscriptionWrite { sender };

            Ok((read, write))
        })
        .await
}

pub struct SubscriptionRead {
    inner: BoxStream<'static, crate::Result<SubEvent<PersistentSubEvent>>>,
}

impl SubscriptionRead {
    pub async fn try_next(&mut self) -> crate::Result<Option<SubEvent<PersistentSubEvent>>> {
        self.inner.try_next().await
    }

    pub async fn try_next_event(&mut self) -> crate::Result<Option<PersistentSubEvent>> {
        loop {
            let event = self.inner.try_next().await?;
            if let Some(event) = event {
                if let SubEvent::EventAppeared(event) = event {
                    return Ok(Some(event));
                }
            } else {
                return Ok(None);
            }
        }
    }
}
fn to_proto_uuid(id: uuid::Uuid) -> Uuid {
    Uuid {
        value: Some(shared::uuid::Value::String(format!("{}", id))),
    }
}

pub struct SubscriptionWrite {
    sender: futures::channel::mpsc::Sender<persistent::ReadReq>,
}

impl SubscriptionWrite {
    pub async fn ack_event(&mut self, event: ResolvedEvent) -> crate::Result<()> {
        self.ack(vec![event.get_original_event().id]).await
    }

    pub async fn ack<I>(&mut self, event_ids: I) -> crate::Result<()>
    where
        I: IntoIterator<Item = uuid::Uuid>,
    {
        use futures::sink::SinkExt;
        use persistent::read_req::{Ack, Content};
        use persistent::ReadReq;

        let ids = event_ids.into_iter().map(to_proto_uuid).collect();
        let ack = Ack {
            id: Vec::new(),
            ids,
        };

        let content = Content::Ack(ack);
        let read_req = ReadReq {
            content: Some(content),
        };

        let _ = self.sender.send(read_req).await;

        Ok(())
    }

    pub async fn nack_event(
        &mut self,
        event: ResolvedEvent,
        action: NakAction,
        reason: impl AsRef<str>,
    ) -> crate::Result<()> {
        self.nack(vec![event.get_original_event().id], action, reason)
            .await
    }

    pub async fn nack<I>(
        &mut self,
        event_ids: I,
        action: NakAction,
        reason: impl AsRef<str>,
    ) -> crate::Result<()>
    where
        I: IntoIterator<Item = uuid::Uuid>,
    {
        use futures::sink::SinkExt;
        use persistent::read_req::{Content, Nack};
        use persistent::ReadReq;

        let ids = event_ids.into_iter().map(to_proto_uuid).collect();

        let action = match action {
            NakAction::Unknown => 0,
            NakAction::Park => 1,
            NakAction::Retry => 2,
            NakAction::Skip => 3,
            NakAction::Stop => 4,
        };

        let nack = Nack {
            id: Vec::new(),
            ids,
            action,
            reason: reason.as_ref().to_string(),
        };

        let content = Content::Nack(nack);
        let read_req = ReadReq {
            content: Some(content),
        };

        let _ = self.sender.send(read_req).await;

        Ok(())
    }
}

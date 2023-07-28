use super::config::C8yMapperConfig;
use super::converter::CumulocityConverter;
use super::dynamic_discovery::process_inotify_events;
use crate::converter::Converter;
use async_trait::async_trait;
use c8y_api::smartrest::topic::SMARTREST_PUBLISH_TOPIC;
use c8y_http_proxy::handle::C8YHttpProxy;
use c8y_http_proxy::messages::C8YRestRequest;
use c8y_http_proxy::messages::C8YRestResult;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration;
use tedge_actors::adapt;
use tedge_actors::fan_in_message_type;
use tedge_actors::Actor;
use tedge_actors::Builder;
use tedge_actors::DynSender;
use tedge_actors::LoggingSender;
use tedge_actors::MessageReceiver;
use tedge_actors::MessageSink;
use tedge_actors::MessageSource;
use tedge_actors::NoConfig;
use tedge_actors::RuntimeError;
use tedge_actors::RuntimeRequest;
use tedge_actors::RuntimeRequestSink;
use tedge_actors::Sender;
use tedge_actors::ServiceProvider;
use tedge_actors::SimpleMessageBox;
use tedge_actors::SimpleMessageBoxBuilder;
use tedge_file_system_ext::FsWatchEvent;
use tedge_mqtt_ext::Message;
use tedge_mqtt_ext::MqttMessage;
use tedge_mqtt_ext::Topic;
use tedge_mqtt_ext::TopicFilter;
use tedge_timer_ext::SetTimeout;
use tedge_timer_ext::Timeout;
use tedge_utils::file::create_directory_with_defaults;
use tedge_utils::file::FileError;

const SYNC_WINDOW: Duration = Duration::from_secs(3);

pub type SyncStart = SetTimeout<()>;
pub type SyncComplete = Timeout<()>;

fan_in_message_type!(C8yMapperInput[MqttMessage, FsWatchEvent, SyncComplete] : Debug);
type C8yMapperOutput = MqttMessage;

pub struct C8yMapperActor {
    converter: CumulocityConverter,
    messages: SimpleMessageBox<C8yMapperInput, C8yMapperOutput>,
    mqtt_publisher: LoggingSender<MqttMessage>,
    timer_sender: LoggingSender<SyncStart>,
    registered_entities: HashMap<String, String>,
}

#[async_trait]
impl Actor for C8yMapperActor {
    fn name(&self) -> &str {
        "CumulocityMapper"
    }

    async fn run(&mut self) -> Result<(), RuntimeError> {
        let init_messages = self.converter.init_messages();
        for init_message in init_messages.into_iter() {
            let _ = self.mqtt_publisher.send(init_message).await?;
        }

        // Start the sync phase
        self.timer_sender
            .send(SyncStart::new(SYNC_WINDOW, ()))
            .await?;

        while let Some(event) = self.messages.recv().await {
            match event {
                C8yMapperInput::MqttMessage(message) => {
                    self.process_mqtt_message(message).await?;
                }
                C8yMapperInput::FsWatchEvent(event) => {
                    self.process_file_watch_event(event).await?;
                }
                C8yMapperInput::SyncComplete(_) => {
                    self.process_sync_timeout().await?;
                }
            }
        }
        Ok(())
    }
}

impl C8yMapperActor {
    pub fn new(
        converter: CumulocityConverter,
        messages: SimpleMessageBox<C8yMapperInput, C8yMapperOutput>,
        mqtt_publisher: LoggingSender<MqttMessage>,
        timer_sender: LoggingSender<SyncStart>,
    ) -> Self {
        Self {
            converter,
            messages,
            mqtt_publisher,
            timer_sender,
            registered_entities: HashMap::new(),
        }
    }

    async fn process_mqtt_message(&mut self, message: MqttMessage) -> Result<(), RuntimeError> {
        // register device if not registered
        if let Some(entity_id) = entity_mqtt_id(&message.topic) {
            if self.registered_entities.get(entity_id).is_none() {
                let register_messages = self.auto_register_entity(entity_id);
                for msg in register_messages {
                    let _ = self.mqtt_publisher.send(msg).await;
                }
            }
        }

        let converted_messages = self.converter.convert(&message).await;

        for converted_message in converted_messages.into_iter() {
            let _ = self.mqtt_publisher.send(converted_message).await;
        }

        Ok(())
    }

    /// Performs auto-registration process for an entity under a given
    /// identifier.
    ///
    /// If an entity is a service, its device is also auto-registered if it's
    /// not already registered.
    ///
    /// It returns MQTT register messages for the given entities to be published
    /// by the mapper, so other components can also be aware of a new device
    /// being registered.
    fn auto_register_entity(&mut self, entity_id: &str) -> Vec<Message> {
        let mut register_messages = vec![];
        if let Some(("te", topic)) = entity_id.split_once('/') {
            let (device_id, service_id) = match topic.split('/').collect::<Vec<&str>>()[..] {
                ["device", device_id, "service", service_id, ..] => (device_id, Some(service_id)),
                ["device", device_id, ..] => (device_id, None),
                _ => return register_messages,
            };

            let device_register_topic = format!("te/device/{device_id}");
            let device_register_payload = r#"{ "@type": "device", "type": "Gateway" }"#.to_string();
            register_messages.push(Message::new(
                &Topic::new(&device_register_topic).unwrap(),
                device_register_payload.clone(),
            ));
            self.register_entity(device_register_topic, device_register_payload);

            if let Some(service_id) = service_id {
                let service_register_topic = format!("te/device/{device_id}/service/{service_id}");
                let service_register_payload =
                    r#"{"@type": "service", "type": "systemd"}"#.to_string();
                register_messages.push(Message::new(
                    &Topic::new(&service_register_topic).unwrap(),
                    service_register_payload.clone(),
                ));
                self.register_entity(service_register_topic, service_register_payload);
            }
        }
        register_messages
    }

    /// Registers the entity under a given MQTT topic.
    ///
    /// If a given entity was registered previously, the function will do
    /// nothing. Otherwise it will save registration data to memory, free to be
    /// queried by other components.
    fn register_entity(&mut self, topic: String, payload: String) {
        self.registered_entities.entry(topic).or_insert(payload);
    }

    async fn process_file_watch_event(
        &mut self,
        file_event: FsWatchEvent,
    ) -> Result<(), RuntimeError> {
        match file_event.clone() {
            FsWatchEvent::DirectoryCreated(path) => {
                if let Some(directory_name) = path.file_name() {
                    let child_id = directory_name.to_string_lossy().to_string();
                    let message = Message::new(
                        &Topic::new_unchecked(SMARTREST_PUBLISH_TOPIC),
                        format!("101,{child_id},{child_id},thin-edge.io-child"),
                    );
                    self.mqtt_publisher.send(message).await?;
                }
            }
            FsWatchEvent::FileCreated(path)
            | FsWatchEvent::FileDeleted(path)
            | FsWatchEvent::Modified(path)
            | FsWatchEvent::DirectoryDeleted(path) => {
                match process_inotify_events(&path, file_event) {
                    Ok(Some(discovered_ops)) => {
                        self.mqtt_publisher
                            .send(
                                self.converter
                                    .process_operation_update_message(discovered_ops),
                            )
                            .await?;
                    }
                    Ok(None) => {}
                    Err(e) => {
                        eprintln!("Processing inotify event failed due to {}", e);
                    }
                }
            }
        }

        Ok(())
    }

    pub async fn process_sync_timeout(&mut self) -> Result<(), RuntimeError> {
        // Once the sync phase is complete, retrieve all sync messages from the converter and process them
        let sync_messages = self.converter.sync_messages();
        for message in sync_messages {
            self.process_mqtt_message(message).await?;
        }

        Ok(())
    }
}

fn entity_mqtt_id(topic: &Topic) -> Option<&str> {
    match topic.name.split('/').collect::<Vec<&str>>()[..] {
        ["te", "device", _device_id, "service", service_id, ..] => {
            Some(&topic.name[..topic.name.find(service_id).unwrap() + service_id.len()])
        }
        ["te", "device", device_id, ..] => {
            Some(&topic.name[..topic.name.find(device_id).unwrap() + device_id.len()])
        }
        _ => None,
    }
}
pub struct C8yMapperBuilder {
    config: C8yMapperConfig,
    box_builder: SimpleMessageBoxBuilder<C8yMapperInput, C8yMapperOutput>,
    mqtt_publisher: DynSender<MqttMessage>,
    http_proxy: C8YHttpProxy,
    timer_sender: DynSender<SyncStart>,
}

impl C8yMapperBuilder {
    pub fn try_new(
        config: C8yMapperConfig,
        mqtt: &mut impl ServiceProvider<MqttMessage, MqttMessage, TopicFilter>,
        http: &mut impl ServiceProvider<C8YRestRequest, C8YRestResult, NoConfig>,
        timer: &mut impl ServiceProvider<SyncStart, SyncComplete, NoConfig>,
        fs_watcher: &mut impl MessageSource<FsWatchEvent, PathBuf>,
    ) -> Result<Self, FileError> {
        Self::init(&config)?;

        let box_builder = SimpleMessageBoxBuilder::new("CumulocityMapper", 16);

        let mqtt_publisher =
            mqtt.connect_consumer(config.topics.clone(), adapt(&box_builder.get_sender()));
        let http_proxy = C8YHttpProxy::new("C8yMapper => C8YHttpProxy", http);
        let timer_sender = timer.connect_consumer(NoConfig, adapt(&box_builder.get_sender()));
        fs_watcher.register_peer(config.ops_dir.clone(), adapt(&box_builder.get_sender()));

        Ok(Self {
            config,
            box_builder,
            mqtt_publisher,
            http_proxy,
            timer_sender,
        })
    }

    fn init(config: &C8yMapperConfig) -> Result<(), FileError> {
        // Create c8y operations directory
        create_directory_with_defaults(config.ops_dir.clone())?;
        // Create directory for device custom fragments
        create_directory_with_defaults(config.config_dir.join("device"))?;
        Ok(())
    }
}

impl RuntimeRequestSink for C8yMapperBuilder {
    fn get_signal_sender(&self) -> DynSender<RuntimeRequest> {
        self.box_builder.get_signal_sender()
    }
}

impl Builder<C8yMapperActor> for C8yMapperBuilder {
    type Error = RuntimeError;

    fn try_build(self) -> Result<C8yMapperActor, Self::Error> {
        let mqtt_publisher = LoggingSender::new("C8yMapper => Mqtt".into(), self.mqtt_publisher);
        let timer_sender = LoggingSender::new("C8yMapper => Timer".into(), self.timer_sender);

        let converter =
            CumulocityConverter::new(self.config, mqtt_publisher.clone(), self.http_proxy)
                .map_err(|err| RuntimeError::ActorError(Box::new(err)))?;

        let message_box = self.box_builder.build();

        Ok(C8yMapperActor::new(
            converter,
            message_box,
            mqtt_publisher,
            timer_sender,
        ))
    }
}

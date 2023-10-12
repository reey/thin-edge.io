use crate::converter::CumulocityConverter;
use crate::error::ConversionError;
use crate::error::CumulocityMapperError;
use crate::error::CumulocityMapperError::UnknownDevice;
use c8y_api::smartrest::smartrest_deserializer::SmartRestConfigDownloadRequest;
use c8y_api::smartrest::smartrest_deserializer::SmartRestConfigUploadRequest;
use c8y_api::smartrest::smartrest_deserializer::SmartRestOperationVariant;
use c8y_api::smartrest::smartrest_deserializer::SmartRestRequestGeneric;
use c8y_api::smartrest::smartrest_serializer::CumulocitySupportedOperations;
use c8y_api::smartrest::smartrest_serializer::SmartRestSerializer;
use c8y_api::smartrest::smartrest_serializer::SmartRestSetOperationToExecuting;
use c8y_api::smartrest::smartrest_serializer::SmartRestSetOperationToFailed;
use c8y_api::smartrest::smartrest_serializer::SmartRestSetOperationToSuccessful;
use nanoid::nanoid;
use sha256::digest;
use std::fs;
use std::io;
use std::os::unix::fs as unix_fs;
use std::path::Path;
use std::sync::Arc;
use tedge_actors::Sender;
use tedge_api::entity_store::EntityMetadata;
use tedge_api::entity_store::EntityType;
use tedge_api::messages::CommandStatus;
use tedge_api::messages::ConfigMetadata;
use tedge_api::messages::ConfigSnapshotCmdPayload;
use tedge_api::messages::ConfigUpdateCmdPayload;
use tedge_api::mqtt_topics::Channel;
use tedge_api::mqtt_topics::ChannelFilter::Command;
use tedge_api::mqtt_topics::ChannelFilter::CommandMetadata;
use tedge_api::mqtt_topics::EntityFilter::AnyEntity;
use tedge_api::mqtt_topics::EntityTopicId;
use tedge_api::mqtt_topics::MqttSchema;
use tedge_api::mqtt_topics::OperationType;
use tedge_api::Jsonify;
use tedge_downloader_ext::DownloadRequest;
use tedge_downloader_ext::DownloadResult;
use tedge_mqtt_ext::Message;
use tedge_mqtt_ext::MqttMessage;
use tedge_mqtt_ext::QoS;
use tedge_mqtt_ext::TopicFilter;
use tedge_utils::file::create_directory_with_defaults;
use tedge_utils::file::create_file_with_defaults;
use tracing::log::info;
use tracing::log::warn;

pub fn config_snapshot_topic_filter(mqtt_schema: &MqttSchema) -> TopicFilter {
    [
        mqtt_schema.topics(AnyEntity, Command(OperationType::ConfigSnapshot)),
        mqtt_schema.topics(AnyEntity, CommandMetadata(OperationType::ConfigSnapshot)),
    ]
    .into_iter()
    .collect()
}

pub fn config_update_topic_filter(mqtt_schema: &MqttSchema) -> TopicFilter {
    [
        mqtt_schema.topics(AnyEntity, Command(OperationType::ConfigUpdate)),
        mqtt_schema.topics(AnyEntity, CommandMetadata(OperationType::ConfigUpdate)),
    ]
    .into_iter()
    .collect()
}

impl CumulocityConverter {
    /// Convert c8y_UploadConfigFile SmartREST request to ThinEdge config_snapshot command.
    /// Command ID is generated here, but it should be replaced by c8y's operation ID in the future.
    pub fn convert_config_snapshot_request(
        &self,
        smartrest: &str,
    ) -> Result<Vec<Message>, CumulocityMapperError> {
        if !self.config.capabilities.config_snapshot {
            warn!("Received a c8y_UploadConfigFile operation, however, config_snapshot feature is disabled");
            return Ok(vec![]);
        }

        let snapshot_request = SmartRestConfigUploadRequest::from_smartrest(smartrest)?;
        let target = self
            .entity_store
            .get_by_external_id(&snapshot_request.device.clone().into())
            .ok_or_else(|| UnknownDevice {
                device_id: snapshot_request.device.to_string(),
            })?;

        let cmd_id = nanoid!();
        let channel = Channel::Command {
            operation: OperationType::ConfigSnapshot,
            cmd_id: cmd_id.clone(),
        };
        let topic = self.mqtt_schema.topic_for(&target.topic_id, &channel);
        let external_id: String = target.external_id.clone().into();

        // Replace '/' with ':' to avoid creating unexpected directories in file transfer repo
        let tedge_url = format!(
            "http://{}/tedge/file-transfer/{}/config_snapshot/{}-{}",
            &self.config.tedge_http_host,
            external_id,
            snapshot_request.config_type.replace('/', ":"),
            cmd_id
        );

        let request = ConfigSnapshotCmdPayload {
            status: CommandStatus::Init,
            tedge_url,
            config_type: snapshot_request.config_type,
            path: None,
        };

        // Command messages must be retained
        Ok(vec![Message::new(&topic, request.to_json()).with_retain()])
    }

    /// Address received ThinEdge config_snapshot command. If its status is
    /// - "executing", it converts the message to SmartREST "Executing".
    /// - "successful", it uploads a config snapshot to c8y and converts the message to SmartREST "Successful".
    /// - "failed", it converts the message to SmartREST "Failed".
    pub async fn handle_config_snapshot_state_change(
        &mut self,
        topic_id: &EntityTopicId,
        cmd_id: &str,
        message: &Message,
    ) -> Result<Vec<Message>, ConversionError> {
        if !self.config.capabilities.config_snapshot {
            warn!(
                "Received a config_snapshot command, however, config_snapshot feature is disabled"
            );
            return Ok(vec![]);
        }

        // get the device metadata from its id
        let device = self.entity_store.get(topic_id).ok_or_else(|| {
            CumulocityMapperError::UnregisteredDevice {
                topic_id: topic_id.to_string(),
            }
        })?;
        let external_id = &device.external_id;
        let smartrest_topic = self.smartrest_publish_topic_for_entity(topic_id)?;
        let payload = message.payload_str()?;
        let response = &ConfigSnapshotCmdPayload::from_json(payload)?;

        let messages = match response.status {
            CommandStatus::Executing => {
                let smartrest_operation_status = SmartRestSetOperationToExecuting::new(
                    CumulocitySupportedOperations::C8yUploadConfigFile,
                )
                .to_smartrest()?;
                vec![Message::new(&smartrest_topic, smartrest_operation_status)]
            }
            CommandStatus::Successful => {
                let uploaded_file_path = self
                    .config
                    .data_dir
                    .file_transfer_dir()
                    .join(device.external_id.as_ref())
                    .join("config_snapshot")
                    .join(format!(
                        "{}-{}",
                        response.config_type.replace('/', ":"),
                        cmd_id
                    ));

                let result = self
                    .http_proxy
                    .upload_file(
                        uploaded_file_path.as_std_path(),
                        &response.config_type,
                        external_id.as_ref().to_string(),
                    )
                    .await; // We need to get rid of this await, otherwise it blocks

                let smartrest_operation_status = match result {
                    Ok(url) => SmartRestSetOperationToSuccessful::new(
                        CumulocitySupportedOperations::C8yUploadConfigFile,
                    )
                    .with_response_parameter(&url)
                    .to_smartrest()?,
                    Err(err) => SmartRestSetOperationToFailed::new(
                        CumulocitySupportedOperations::C8yUploadConfigFile,
                        format!("Upload failed with {}", err),
                    )
                    .to_smartrest()?,
                };

                let c8y_notification = Message::new(&smartrest_topic, smartrest_operation_status);
                let clear_local_cmd = Message::new(&message.topic, "")
                    .with_retain()
                    .with_qos(QoS::AtLeastOnce);
                vec![c8y_notification, clear_local_cmd]
            }
            CommandStatus::Failed { ref reason } => {
                let smartrest_operation_status = SmartRestSetOperationToFailed::new(
                    CumulocitySupportedOperations::C8yUploadConfigFile,
                    reason.clone(),
                )
                .to_smartrest()?;
                let c8y_notification = Message::new(&smartrest_topic, smartrest_operation_status);
                let clear_local_cmd = Message::new(&message.topic, "")
                    .with_retain()
                    .with_qos(QoS::AtLeastOnce);
                vec![c8y_notification, clear_local_cmd]
            }
            _ => {
                vec![] // Do nothing as other components might handle those states
            }
        };

        Ok(messages)
    }

    /// Converts a config_snapshot metadata message to
    /// - supported operation "c8y_UploadConfigFile"
    /// - supported config types
    pub fn convert_config_snapshot_metadata(
        &mut self,
        topic_id: &EntityTopicId,
        message: &Message,
    ) -> Result<Vec<Message>, ConversionError> {
        if !self.config.capabilities.config_snapshot {
            warn!(
                "Received config_snapshot metadata, however, config_snapshot feature is disabled"
            );
        }
        self.convert_config_metadata(topic_id, message, "c8y_UploadConfigFile")
    }

    /// Upon receiving a SmartREST c8y_DownloadConfigFile request,
    /// - Create a download request if the target file is not available in cache.
    /// - If the file is already available, proceed to create a new ThinEdge config_update command.
    /// Command ID is generated here, but it should be replaced by c8y's operation ID in the future.
    pub async fn convert_config_update_request(
        &mut self,
        smartrest: &str,
    ) -> Result<Vec<Message>, CumulocityMapperError> {
        if !self.config.capabilities.config_update {
            warn!(
                "Received a c8y_DownloadConfigFile operation, however, config_update feature is disabled"
            );
            return Ok(vec![]);
        }

        let smartrest = SmartRestConfigDownloadRequest::from_smartrest(smartrest)?;
        let entity = self
            .entity_store
            .get_by_external_id(&smartrest.device.clone().into())
            .ok_or_else(|| UnknownDevice {
                device_id: smartrest.device.clone(),
            })?;

        let cmd_id = nanoid!();
        let remote_url = smartrest.url.as_str();
        let file_cache_key = digest(remote_url);
        let file_cache_path = self.config.data_dir.cache_dir().join(file_cache_key);

        if file_cache_path.is_file() {
            // No download. Create a symlink and config_update command.
            info!("Hit the file cache={file_cache_path}. Create a symlink to the file");
            self.create_symlink_for_config_update(
                entity,
                &smartrest.config_type,
                &cmd_id,
                file_cache_path,
            )?;

            let message = self.create_config_update_cmd(cmd_id.into(), &smartrest, entity);
            Ok(message)
        } else {
            // Require file download
            // Send a request to the Downloader to download the file asynchronously.
            let download_request =
                if let Some(cumulocity_url) = self.c8y_endpoint.maybe_tenant_url(remote_url) {
                    DownloadRequest::new(
                        self.auth_proxy.proxy_url(cumulocity_url).as_ref(),
                        file_cache_path.as_std_path(),
                    )
                } else {
                    DownloadRequest::new(remote_url, file_cache_path.as_std_path())
                };

            self.downloader_sender
                .send((cmd_id.clone(), download_request))
                .await?;
            info!("Awaiting config download for cmd_id: {cmd_id} from url: {remote_url}");

            self.pending_operations.insert(
                cmd_id,
                SmartRestOperationVariant::DownloadConfigFile(smartrest),
            );

            Ok(vec![])
        }
    }

    /// This function is called after DownloaderActor returns the result.
    /// If the result is
    /// - Ok, create a new config_update command.
    /// - Err, create SmartREST Executing and Failed messages to move the operation to end state.
    pub async fn process_download_result_for_config_update(
        &mut self,
        cmd_id: Arc<str>,
        smartrest: &SmartRestConfigDownloadRequest,
        download_result: DownloadResult,
    ) -> Result<Vec<Message>, ConversionError> {
        let device = self
            .entity_store
            .get_by_external_id(&smartrest.device.clone().into())
            .ok_or_else(|| UnknownDevice {
                device_id: smartrest.device.to_string(),
            })?;

        match download_result {
            Ok(download_response) => {
                self.create_symlink_for_config_update(
                    device,
                    &smartrest.config_type,
                    &cmd_id,
                    download_response.file_path,
                )?;
                let message = self.create_config_update_cmd(cmd_id, smartrest, device);
                Ok(message)
            }
            Err(download_err) => {
                let sm_topic = self.smartrest_publish_topic_for_entity(&device.topic_id)?;
                let smartrest_executing = SmartRestSetOperationToExecuting::new(
                    CumulocitySupportedOperations::C8yDownloadConfigFile,
                )
                .to_smartrest()?;
                let smartrest_failed = SmartRestSetOperationToFailed::new(
                    CumulocitySupportedOperations::C8yDownloadConfigFile,
                    format!(
                        "Download from {} failed with {}",
                        smartrest.url, download_err
                    ),
                )
                .to_smartrest()?;

                Ok(vec![
                    Message::new(&sm_topic, smartrest_executing),
                    Message::new(&sm_topic, smartrest_failed),
                ])
            }
        }
    }

    /// Address a received ThinEdge config_update command. If its status is
    /// - "executing", it converts the message to SmartREST "Executing".
    /// - "successful", it converts the message to SmartREST "Successful".
    /// - "failed", it converts the message to SmartREST "Failed".
    /// Remove the symlink when the status is either successful or failed.
    pub async fn handle_config_update_state_change(
        &mut self,
        topic_id: &EntityTopicId,
        cmd_id: &str,
        message: &Message,
    ) -> Result<Vec<Message>, ConversionError> {
        if !self.config.capabilities.config_update {
            warn!("Received a config_update command, however, config_update feature is disabled");
            return Ok(vec![]);
        }

        let device = self.entity_store.get(topic_id).ok_or_else(|| {
            CumulocityMapperError::UnregisteredDevice {
                topic_id: topic_id.to_string(),
            }
        })?;
        let sm_topic = self.smartrest_publish_topic_for_entity(topic_id)?;
        let payload = message.payload_str()?;
        let response = &ConfigUpdateCmdPayload::from_json(payload)?;

        let messages = match response.status {
            CommandStatus::Executing => {
                let smartrest_operation_status = SmartRestSetOperationToExecuting::new(
                    CumulocitySupportedOperations::C8yDownloadConfigFile,
                )
                .to_smartrest()?;
                vec![Message::new(&sm_topic, smartrest_operation_status)]
            }
            CommandStatus::Successful => {
                let smartrest_operation_status = SmartRestSetOperationToSuccessful::new(
                    CumulocitySupportedOperations::C8yDownloadConfigFile,
                )
                .to_smartrest()?;
                let c8y_notification = Message::new(&sm_topic, smartrest_operation_status);
                let clear_local_cmd = Message::new(&message.topic, "")
                    .with_retain()
                    .with_qos(QoS::AtLeastOnce);

                self.delete_symlink_for_config_update(device, &response.config_type, cmd_id)?;

                vec![c8y_notification, clear_local_cmd]
            }
            CommandStatus::Failed { ref reason } => {
                let smartrest_operation_status = SmartRestSetOperationToFailed::new(
                    CumulocitySupportedOperations::C8yDownloadConfigFile,
                    reason.clone(),
                )
                .to_smartrest()?;
                let c8y_notification = Message::new(&sm_topic, smartrest_operation_status);
                let clear_local_cmd = Message::new(&message.topic, "")
                    .with_retain()
                    .with_qos(QoS::AtLeastOnce);

                self.delete_symlink_for_config_update(device, &response.config_type, cmd_id)?;

                vec![c8y_notification, clear_local_cmd]
            }
            _ => {
                vec![] // Do nothing as other components might handle those states
            }
        };

        Ok(messages)
    }

    /// Converts a config_update metadata message to
    /// - supported operation "c8y_DownloadConfigFile"
    /// - supported config types
    pub fn convert_config_update_metadata(
        &mut self,
        topic_id: &EntityTopicId,
        message: &Message,
    ) -> Result<Vec<Message>, ConversionError> {
        if !self.config.capabilities.config_update {
            warn!("Received config_update metadata, however, config_update feature is disabled");
            return Ok(vec![]);
        }
        self.convert_config_metadata(topic_id, message, "c8y_DownloadConfigFile")
    }

    fn convert_config_metadata(
        &mut self,
        topic_id: &EntityTopicId,
        message: &Message,
        c8y_op_name: &str,
    ) -> Result<Vec<Message>, ConversionError> {
        let metadata = ConfigMetadata::from_json(message.payload_str()?)?;

        // get the device metadata from its id
        let device = self.entity_store.get(topic_id).ok_or_else(|| {
            CumulocityMapperError::UnregisteredDevice {
                topic_id: topic_id.to_string(),
            }
        })?;

        // Create a c8y operation file
        let dir_path = match device.r#type {
            EntityType::MainDevice => self.ops_dir.clone(),
            EntityType::ChildDevice => {
                match &device.parent {
                    Some(parent) if parent.is_default_main_device() => {
                        // Support only first level child devices due to the limitation of our file system supported operations scheme.
                        self.ops_dir.join(device.external_id.as_ref())
                    }
                    _ => {
                        warn!("config_snapshot and config_update features for nested child devices are currently unsupported");
                        return Ok(vec![]);
                    }
                }
            }
            EntityType::Service => {
                warn!("config_snapshot and config_update features for services are currently unsupported");
                return Ok(vec![]);
            }
        };
        create_directory_with_defaults(&dir_path)?;
        create_file_with_defaults(dir_path.join(c8y_op_name), None)?;

        // To SmartREST supported config types
        let mut types = metadata.types;
        types.sort();
        let supported_config_types = types.join(",");
        let payload = format!("119,{supported_config_types}");

        let sm_topic = self.smartrest_publish_topic_for_entity(topic_id)?;
        Ok(vec![MqttMessage::new(&sm_topic, payload)])
    }

    fn create_config_update_cmd(
        &self,
        cmd_id: Arc<str>,
        smartrest: &SmartRestConfigDownloadRequest,
        target: &EntityMetadata,
    ) -> Vec<Message> {
        let channel = Channel::Command {
            operation: OperationType::ConfigUpdate,
            cmd_id: cmd_id.to_string(),
        };
        let topic = self.mqtt_schema.topic_for(&target.topic_id, &channel);
        let external_id: String = target.external_id.clone().into();

        // Replace '/' with ':' to avoid creating unexpected directories in file transfer repo
        let tedge_url = format!(
            "http://{}/tedge/file-transfer/{}/config_update/{}-{}",
            &self.config.tedge_http_host,
            external_id,
            smartrest.config_type.replace('/', ":"),
            cmd_id
        );

        let request = ConfigUpdateCmdPayload {
            status: CommandStatus::Init,
            tedge_url,
            remote_url: smartrest.url.clone(),
            config_type: smartrest.config_type.clone(),
            path: None,
        };

        // Command messages must be retained
        vec![Message::new(&topic, request.to_json()).with_retain()]
    }

    fn create_symlink_for_config_update(
        &self,
        entity: &EntityMetadata,
        config_type: &str,
        cmd_id: &str,
        original: impl AsRef<Path>,
    ) -> Result<(), io::Error> {
        let symlink_dir_path = self
            .config
            .data_dir
            .file_transfer_dir()
            .join(entity.external_id.as_ref())
            .join("config_update");
        let symlink_path =
            symlink_dir_path.join(format!("{}-{cmd_id}", config_type.replace('/', ":")));

        if !symlink_path.is_symlink() {
            fs::create_dir_all(symlink_dir_path)?;
            unix_fs::symlink(original, &symlink_path)?;
        }

        Ok(())
    }

    fn delete_symlink_for_config_update(
        &self,
        entity: &EntityMetadata,
        config_type: &str,
        cmd_id: &str,
    ) -> Result<(), io::Error> {
        let symlink_dir_path = self
            .config
            .data_dir
            .file_transfer_dir()
            .join(entity.external_id.as_ref())
            .join("config_update");
        let symlink_path =
            symlink_dir_path.join(format!("{}-{cmd_id}", config_type.replace('/', ":")));

        if symlink_path.exists() {
            fs::remove_file(symlink_path)?
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::tests::skip_init_messages;
    use crate::tests::spawn_c8y_mapper_actor;
    use crate::tests::spawn_dummy_c8y_http_proxy;
    use c8y_api::smartrest::topic::C8yTopic;
    use serde_json::json;
    use sha256::digest;
    use std::time::Duration;
    use tedge_actors::test_helpers::MessageReceiverExt;
    use tedge_actors::MessageReceiver;
    use tedge_actors::Sender;
    use tedge_api::mqtt_topics::Channel;
    use tedge_api::mqtt_topics::MqttSchema;
    use tedge_api::mqtt_topics::OperationType;
    use tedge_downloader_ext::DownloadResponse;
    use tedge_mqtt_ext::test_helpers::assert_received_contains_str;
    use tedge_mqtt_ext::MqttMessage;
    use tedge_mqtt_ext::Topic;
    use tedge_test_utils::fs::TempTedgeDir;

    const TEST_TIMEOUT_MS: Duration = Duration::from_millis(5000);

    #[tokio::test]
    async fn mapper_converts_config_metadata_to_supported_op_and_types_for_main_device() {
        let ttd = TempTedgeDir::new();
        let (mqtt, _http, _fs, _timer, _dl) = spawn_c8y_mapper_actor(&ttd, true).await;
        let mut mqtt = mqtt.with_timeout(TEST_TIMEOUT_MS);

        skip_init_messages(&mut mqtt).await;

        // Simulate config_snapshot cmd metadata message
        mqtt.send(MqttMessage::new(
            &Topic::new_unchecked("te/device/main///cmd/config_snapshot"),
            r#"{"types" : [ "typeA", "typeB", "typeC" ]}"#,
        ))
        .await
        .expect("Send failed");

        // Validate SmartREST message is published
        assert_received_contains_str(&mut mqtt, [("c8y/s/us", "119,typeA,typeB,typeC")]).await;

        // Validate if the supported operation file is created
        assert!(ttd
            .path()
            .join("operations/c8y/c8y_UploadConfigFile")
            .exists());

        // Simulate config_update cmd metadata message
        mqtt.send(MqttMessage::new(
            &Topic::new_unchecked("te/device/main///cmd/config_update"),
            r#"{"types" : [ "typeD", "typeE", "typeF" ]}"#,
        ))
        .await
        .expect("Send failed");

        // Validate SmartREST message is published
        assert_received_contains_str(&mut mqtt, [("c8y/s/us", "119,typeD,typeE,typeF")]).await;

        // Validate if the supported operation file is created
        assert!(ttd
            .path()
            .join("operations/c8y/c8y_DownloadConfigFile")
            .exists());
    }

    #[tokio::test]
    async fn mapper_converts_config_cmd_to_supported_op_and_types_for_child_device() {
        let ttd = TempTedgeDir::new();
        let (mqtt, _http, _fs, _timer, _dl) = spawn_c8y_mapper_actor(&ttd, true).await;
        let mut mqtt = mqtt.with_timeout(TEST_TIMEOUT_MS);

        skip_init_messages(&mut mqtt).await;

        // Simulate config_snapshot cmd metadata message
        mqtt.send(MqttMessage::new(
            &Topic::new_unchecked("te/device/child1///cmd/config_snapshot"),
            r#"{"types" : [ "typeA", "typeB", "typeC" ]}"#,
        ))
        .await
        .expect("Send failed");

        mqtt.skip(2).await; // Skip the mapped child device registration message

        // Validate SmartREST message is published
        assert_received_contains_str(
            &mut mqtt,
            [(
                "c8y/s/us/test-device:device:child1",
                "119,typeA,typeB,typeC",
            )],
        )
        .await;

        // Validate if the supported operation file is created
        assert!(ttd
            .path()
            .join("operations/c8y/test-device:device:child1/c8y_UploadConfigFile")
            .exists());

        // Simulate config_update cmd metadata message
        mqtt.send(MqttMessage::new(
            &Topic::new_unchecked("te/device/child1///cmd/config_update"),
            r#"{"types" : [ "typeD", "typeE", "typeF" ]}"#,
        ))
        .await
        .expect("Send failed");

        // Validate SmartREST message is published
        assert_received_contains_str(
            &mut mqtt,
            [(
                "c8y/s/us/test-device:device:child1",
                "119,typeD,typeE,typeF",
            )],
        )
        .await;

        // Validate if the supported operation file is created
        assert!(ttd
            .path()
            .join("operations/c8y/test-device:device:child1/c8y_DownloadConfigFile")
            .exists());
    }

    #[tokio::test]
    async fn mapper_converts_smartrest_config_upload_req_to_config_snapshot_cmd_for_main_device() {
        let ttd = TempTedgeDir::new();
        let (mqtt, _http, _fs, _timer, _dl) = spawn_c8y_mapper_actor(&ttd, true).await;
        let mut mqtt = mqtt.with_timeout(TEST_TIMEOUT_MS);

        skip_init_messages(&mut mqtt).await;

        // Simulate c8y_UploadConfigFile SmartREST request
        mqtt.send(MqttMessage::new(
            &C8yTopic::downstream_topic(),
            "526,test-device,path/config/A",
        ))
        .await
        .expect("Send failed");

        let (topic, received_json) = mqtt
            .recv()
            .await
            .map(|msg| {
                (
                    msg.topic,
                    serde_json::from_str::<serde_json::Value>(msg.payload.as_str().expect("UTF8"))
                        .expect("JSON"),
                )
            })
            .unwrap();

        let mqtt_schema = MqttSchema::default();
        let (entity, channel) = mqtt_schema.entity_channel_of(&topic).unwrap();
        assert_eq!(entity, "device/main//");

        if let Channel::Command {
            operation: OperationType::ConfigSnapshot,
            cmd_id,
        } = channel
        {
            // Validate the payload JSON
            let expected_json = json!({
                "status": "init",
                "tedgeUrl": format!("http://localhost:8888/tedge/file-transfer/test-device/config_snapshot/path:config:A-{cmd_id}"),
                "type": "path/config/A",
            });
            assert_json_diff::assert_json_include!(actual: received_json, expected: expected_json);
        } else {
            panic!("Unexpected response on channel: {:?}", topic)
        }
    }

    #[tokio::test]
    async fn mapper_converts_smartrest_config_upload_req_to_config_snapshot_cmd_for_child_device() {
        let ttd = TempTedgeDir::new();
        let (mqtt, _http, _fs, _timer, _dl) = spawn_c8y_mapper_actor(&ttd, true).await;
        let mut mqtt = mqtt.with_timeout(TEST_TIMEOUT_MS);

        skip_init_messages(&mut mqtt).await;

        // The child device must be registered first
        mqtt.send(MqttMessage::new(
            &Topic::new_unchecked("te/device/child1//"),
            r#"{ "@type":"child-device", "@id":"child1" }"#,
        ))
        .await
        .expect("fail to register the child-device");

        mqtt.skip(2).await; // Skip the mapped child device registration message

        // Simulate c8y_UploadConfigFile SmartREST request
        mqtt.send(MqttMessage::new(
            &C8yTopic::downstream_topic(),
            "526,child1,configA",
        ))
        .await
        .expect("Send failed");

        let (topic, received_json) = mqtt
            .recv()
            .await
            .map(|msg| {
                (
                    msg.topic,
                    serde_json::from_str::<serde_json::Value>(msg.payload.as_str().expect("UTF8"))
                        .expect("JSON"),
                )
            })
            .unwrap();

        let mqtt_schema = MqttSchema::default();
        let (entity, channel) = mqtt_schema.entity_channel_of(&topic).unwrap();
        assert_eq!(entity, "device/child1//");

        if let Channel::Command {
            operation: OperationType::ConfigSnapshot,
            cmd_id,
        } = channel
        {
            // Validate the payload JSON
            let expected_json = json!({
                "status": "init",
                "tedgeUrl": format!("http://localhost:8888/tedge/file-transfer/child1/config_snapshot/configA-{cmd_id}"),
                "type": "configA",
            });
            assert_json_diff::assert_json_include!(actual: received_json, expected: expected_json);
        } else {
            panic!("Unexpected response on channel: {:?}", topic)
        }
    }

    #[tokio::test]
    async fn handle_config_snapshot_executing_and_failed_cmd_for_main_device() {
        let ttd = TempTedgeDir::new();
        let (mqtt, _http, _fs, _timer, _dl) = spawn_c8y_mapper_actor(&ttd, true).await;
        let mut mqtt = mqtt.with_timeout(TEST_TIMEOUT_MS);

        skip_init_messages(&mut mqtt).await;

        // Simulate config_snapshot command with "executing" state
        mqtt.send(MqttMessage::new(
            &Topic::new_unchecked("te/device/main///cmd/config_snapshot/1234"),
            json!({
            "status": "executing",
            "tedgeUrl": "http://localhost:8888/tedge/file-transfer/test-device/config_snapshot/typeA-1234",
            "type": "typeA",
        })
                .to_string(),
        ))
            .await
            .expect("Send failed");

        // Expect `501` smartrest message on `c8y/s/us`.
        assert_received_contains_str(&mut mqtt, [("c8y/s/us", "501,c8y_UploadConfigFile")]).await;

        // Simulate config_snapshot command with "failed" state
        mqtt.send(MqttMessage::new(
            &Topic::new_unchecked("te/device/main///cmd/config_snapshot/1234"),
            json!({
            "status": "failed",
            "tedgeUrl": "http://localhost:8888/tedge/file-transfer/test-device/config_snapshot/typeA-1234",
            "type": "typeA",
            "reason": "Something went wrong"
        })
                .to_string(),
        ))
            .await
            .expect("Send failed");

        // Expect `502` smartrest message on `c8y/s/us`.
        assert_received_contains_str(
            &mut mqtt,
            [(
                "c8y/s/us",
                "502,c8y_UploadConfigFile,\"Something went wrong\"",
            )],
        )
        .await;
    }

    #[tokio::test]
    async fn handle_config_snapshot_executing_and_failed_cmd_for_child_device() {
        let ttd = TempTedgeDir::new();
        let (mqtt, _http, _fs, _timer, _dl) = spawn_c8y_mapper_actor(&ttd, true).await;
        let mut mqtt = mqtt.with_timeout(TEST_TIMEOUT_MS);

        skip_init_messages(&mut mqtt).await;

        // The child device must be registered first
        mqtt.send(MqttMessage::new(
            &Topic::new_unchecked("te/device/child1//"),
            r#"{ "@type":"child-device", "@id":"child1" }"#,
        ))
        .await
        .expect("fail to register the child-device");

        mqtt.skip(2).await; // Skip child device registration messages

        // Simulate config_snapshot command with "executing" state
        mqtt.send(MqttMessage::new(
            &Topic::new_unchecked("te/device/child1///cmd/config_snapshot/1234"),
            json!({
            "status": "executing",
            "tedgeUrl": "http://localhost:8888/tedge/file-transfer/child1/config_snapshot/typeA-1234",
            "type": "typeA",
        })
                .to_string(),
        ))
            .await
            .expect("Send failed");

        // Expect `501` smartrest message on child topic.
        assert_received_contains_str(&mut mqtt, [("c8y/s/us/child1", "501,c8y_UploadConfigFile")])
            .await;

        // Simulate config_snapshot command with "failed" state
        mqtt.send(MqttMessage::new(
            &Topic::new_unchecked("te/device/child1///cmd/config_snapshot/1234"),
            json!({
            "status": "failed",
            "tedgeUrl": format!("http://localhost:8888/tedge/file-transfer/child1/config_snapshot/typeA-1234"),
            "type": "typeA",
            "reason": "Something went wrong"
        })
                .to_string(),
        ))
            .await
            .expect("Send failed");

        // Expect `502` smartrest message on child topic.
        assert_received_contains_str(
            &mut mqtt,
            [(
                "c8y/s/us/child1",
                "502,c8y_UploadConfigFile,\"Something went wrong\"",
            )],
        )
        .await;
    }

    #[tokio::test]
    async fn handle_config_snapshot_successful_cmd_for_main_device() {
        let ttd = TempTedgeDir::new();
        let (mqtt, http, _fs, _timer, _dl) = spawn_c8y_mapper_actor(&ttd, true).await;
        spawn_dummy_c8y_http_proxy(http);
        let mut mqtt = mqtt.with_timeout(TEST_TIMEOUT_MS);

        skip_init_messages(&mut mqtt).await;

        // Simulate a config file is uploaded to the file transfer repository
        ttd.dir("file-transfer")
            .dir("test-device")
            .dir("config_snapshot")
            .file("path:type:A-1234");

        // Simulate config_snapshot command with "executing" state
        mqtt.send(MqttMessage::new(
            &Topic::new_unchecked("te/device/main///cmd/config_snapshot/1234"),
            json!({
            "status": "successful",
            "tedgeUrl": "http://localhost:8888/tedge/file-transfer/test-device/config_snapshot/path:type:A-1234",
            "type": "path/type/A",
        })
                .to_string(),
        ))
            .await
            .expect("Send failed");

        // Expect `503` smartrest message on `c8y/s/us`.
        assert_received_contains_str(
            &mut mqtt,
            [("c8y/s/us", "503,c8y_UploadConfigFile,http://c8y-binary.url")],
        )
        .await;
    }

    #[tokio::test]
    async fn handle_config_snapshot_successful_cmd_for_child_device() {
        let ttd = TempTedgeDir::new();
        let (mqtt, http, _fs, _timer, _dl) = spawn_c8y_mapper_actor(&ttd, true).await;
        spawn_dummy_c8y_http_proxy(http);
        let mut mqtt = mqtt.with_timeout(TEST_TIMEOUT_MS);

        skip_init_messages(&mut mqtt).await;

        // The child device must be registered first
        mqtt.send(MqttMessage::new(
            &Topic::new_unchecked("te/device/child1//"),
            r#"{ "@type":"child-device", "@id":"child1" }"#,
        ))
        .await
        .expect("fail to register the child-device");

        mqtt.skip(2).await; // Skip child device registration messages

        // Simulate a config file is uploaded to the file transfer repository
        ttd.dir("file-transfer")
            .dir("child1")
            .dir("config_snapshot")
            .file("typeA-1234");

        // Simulate config_snapshot command with "executing" state
        mqtt.send(MqttMessage::new(
            &Topic::new_unchecked("te/device/child1///cmd/config_snapshot/1234"),
            json!({
            "status": "successful",
            "tedgeUrl": "http://localhost:8888/tedge/file-transfer/child1/config_snapshot/typeA-1234",
            "type": "typeA",
        })
                .to_string(),
        ))
            .await
            .expect("Send failed");

        // Expect `503` smartrest message on child topic.
        assert_received_contains_str(
            &mut mqtt,
            [(
                "c8y/s/us/child1",
                "503,c8y_UploadConfigFile,http://c8y-binary.url",
            )],
        )
        .await;
    }

    #[tokio::test]
    async fn mapper_converts_smartrest_config_download_req_with_new_download_for_main_device() {
        let ttd = TempTedgeDir::new();
        let (mqtt, _http, _fs, _timer, mut dl) = spawn_c8y_mapper_actor(&ttd, true).await;
        let mut mqtt = mqtt.with_timeout(TEST_TIMEOUT_MS);

        skip_init_messages(&mut mqtt).await;

        // Simulate c8y_DownloadConfigFile SmartREST request
        mqtt.send(MqttMessage::new(
            &C8yTopic::downstream_topic(),
            "524,test-device,http://www.my.url,path/config/A",
        ))
        .await
        .expect("Send failed");

        // Assert download request
        let download_path = ttd.path().join("cache").join(digest("http://www.my.url"));
        let (cmd_id, download_request) = dl.recv().await.unwrap();
        assert_eq!(download_request.url, "http://www.my.url");
        assert_eq!(download_request.file_path, download_path);
        assert_eq!(download_request.auth, None);

        // Simulate downloading a file is completed
        ttd.dir("cache").file(&digest("http://www.my.url"));
        let download_response = DownloadResponse::new("http://www.my.url", &download_path);
        dl.send((cmd_id.clone(), Ok(download_response)))
            .await
            .unwrap();

        // New config_update command should be published
        let (topic, received_json) = mqtt
            .recv()
            .await
            .map(|msg| {
                (
                    msg.topic,
                    serde_json::from_str::<serde_json::Value>(msg.payload.as_str().expect("UTF8"))
                        .expect("JSON"),
                )
            })
            .unwrap();

        let mqtt_schema = MqttSchema::default();
        let (entity, channel) = mqtt_schema.entity_channel_of(&topic).unwrap();
        assert_eq!(entity, "device/main//");

        if let Channel::Command {
            operation: OperationType::ConfigUpdate,
            cmd_id,
        } = channel
        {
            // Assert symlink is created
            assert!(ttd
                .path()
                .join(format!(
                    "file-transfer/test-device/config_update/path:config:A-{cmd_id}"
                ))
                .is_symlink());

            // Validate the payload JSON
            let expected_json = json!({
                "status": "init",
                "tedgeUrl": format!("http://localhost:8888/tedge/file-transfer/test-device/config_update/path:config:A-{cmd_id}"),
                "type": "path/config/A",
            });
            assert_json_diff::assert_json_include!(actual: received_json, expected: expected_json);
        } else {
            panic!("Unexpected response on channel: {:?}", topic)
        }
    }

    #[tokio::test]
    async fn mapper_converts_smartrest_config_download_req_without_new_download_for_child_device() {
        let ttd = TempTedgeDir::new();
        let (mqtt, _http, _fs, _timer, _dl) = spawn_c8y_mapper_actor(&ttd, true).await;
        let mut mqtt = mqtt.with_timeout(TEST_TIMEOUT_MS);

        skip_init_messages(&mut mqtt).await;

        // The child device must be registered first
        mqtt.send(MqttMessage::new(
            &Topic::new_unchecked("te/device/child1//"),
            r#"{ "@type":"child-device", "@id":"child1" }"#,
        ))
        .await
        .expect("fail to register the child-device");

        mqtt.skip(2).await; // Skip child device registration messages

        // Cache is already available
        ttd.dir("cache").file(&digest("http://www.my.url"));

        // Simulate c8y_DownloadConfigFile SmartREST request
        mqtt.send(MqttMessage::new(
            &C8yTopic::downstream_topic(),
            "524,child1,http://www.my.url,configA",
        ))
        .await
        .expect("Send failed");

        // New config_update command should be published
        let (topic, received_json) = mqtt
            .recv()
            .await
            .map(|msg| {
                (
                    msg.topic,
                    serde_json::from_str::<serde_json::Value>(msg.payload.as_str().expect("UTF8"))
                        .expect("JSON"),
                )
            })
            .unwrap();

        let mqtt_schema = MqttSchema::default();
        let (entity, channel) = mqtt_schema.entity_channel_of(&topic).unwrap();
        assert_eq!(entity, "device/child1//");

        if let Channel::Command {
            operation: OperationType::ConfigUpdate,
            cmd_id,
        } = channel
        {
            // Assert symlink is created
            assert!(ttd
                .path()
                .join(format!(
                    "file-transfer/child1/config_update/configA-{cmd_id}"
                ))
                .is_symlink());

            // Validate the payload JSON
            let expected_json = json!({
                "status": "init",
                "tedgeUrl": format!("http://localhost:8888/tedge/file-transfer/child1/config_update/configA-{cmd_id}"),
                "type": "configA",
            });
            assert_json_diff::assert_json_include!(actual: received_json, expected: expected_json);
        } else {
            panic!("Unexpected response on channel: {:?}", topic)
        }
    }

    #[tokio::test]
    async fn handle_config_update_executing_and_failed_cmd_for_main_device() {
        let ttd = TempTedgeDir::new();
        let (mqtt, _http, _fs, _timer, _dl) = spawn_c8y_mapper_actor(&ttd, true).await;
        let mut mqtt = mqtt.with_timeout(TEST_TIMEOUT_MS);

        skip_init_messages(&mut mqtt).await;

        // Simulate a symlink exists
        ttd.dir("file-transfer")
            .dir("test-device")
            .dir("config_update")
            .file("typeA-1234");
        assert!(ttd
            .path()
            .join("file-transfer/test-device/config_update/typeA-1234")
            .exists());

        // Simulate config_snapshot command with "executing" state
        mqtt.send(MqttMessage::new(
            &Topic::new_unchecked("te/device/main///cmd/config_update/1234"),
            json!({
            "status": "executing",
            "tedgeUrl": "http://localhost:8888/tedge/file-transfer/test-device/config_update/typeA-1234",
            "remoteUrl": "http://www.my.url",
            "type": "typeA",
        })
                .to_string(),
        ))
            .await
            .expect("Send failed");

        // Expect `501` smartrest message on `c8y/s/us`.
        assert_received_contains_str(&mut mqtt, [("c8y/s/us", "501,c8y_DownloadConfigFile")]).await;

        // Simulate config_update command with "failed" state
        mqtt.send(MqttMessage::new(
            &Topic::new_unchecked("te/device/main///cmd/config_update/1234"),
            json!({
            "status": "failed",
            "tedgeUrl": "http://localhost:8888/tedge/file-transfer/test-device/config_update/typeA-1234",
            "remoteUrl": "http://www.my.url",
            "type": "typeA",
            "reason": "Something went wrong"
        })
                .to_string(),
        ))
            .await
            .expect("Send failed");

        // Expect `502` smartrest message on `c8y/s/us`.
        assert_received_contains_str(
            &mut mqtt,
            [(
                "c8y/s/us",
                "502,c8y_DownloadConfigFile,\"Something went wrong\"",
            )],
        )
        .await;

        // Assert symlink is removed
        assert!(!ttd
            .path()
            .join("file-transfer/test-device/config_update/typeA-1234")
            .exists());
    }

    #[tokio::test]
    async fn handle_config_update_executing_and_failed_cmd_for_child_device() {
        let ttd = TempTedgeDir::new();
        let (mqtt, _http, _fs, _timer, _dl) = spawn_c8y_mapper_actor(&ttd, true).await;
        let mut mqtt = mqtt.with_timeout(TEST_TIMEOUT_MS);

        skip_init_messages(&mut mqtt).await;

        // The child device must be registered first
        mqtt.send(MqttMessage::new(
            &Topic::new_unchecked("te/device/child1//"),
            r#"{ "@type":"child-device", "@id":"child1" }"#,
        ))
        .await
        .expect("fail to register the child-device");

        mqtt.skip(2).await; // Skip child device registration messages

        // Simulate config_snapshot command with "executing" state
        mqtt.send(MqttMessage::new(
            &Topic::new_unchecked("te/device/child1///cmd/config_update/1234"),
            json!({
            "status": "executing",
            "tedgeUrl": "http://localhost:8888/tedge/file-transfer/child1/config_update/typeA-1234",
            "remoteUrl": "http://www.my.url",
            "type": "typeA",
        })
                .to_string(),
        ))
            .await
            .expect("Send failed");

        // Expect `501` smartrest message on child topic.
        assert_received_contains_str(
            &mut mqtt,
            [("c8y/s/us/child1", "501,c8y_DownloadConfigFile")],
        )
        .await;

        // Simulate config_update command with "failed" state
        mqtt.send(MqttMessage::new(
            &Topic::new_unchecked("te/device/child1///cmd/config_update/1234"),
            json!({
            "status": "failed",
            "tedgeUrl": "http://localhost:8888/tedge/file-transfer/child1/config_update/typeA-1234",
            "remoteUrl": "http://www.my.url",
            "type": "typeA",
            "reason": "Something went wrong"
        })
                .to_string(),
        ))
            .await
            .expect("Send failed");

        // Expect `502` smartrest message on child topic.
        assert_received_contains_str(
            &mut mqtt,
            [(
                "c8y/s/us/child1",
                "502,c8y_DownloadConfigFile,\"Something went wrong\"",
            )],
        )
        .await;
    }

    #[tokio::test]
    async fn handle_config_update_successful_cmd_for_main_device() {
        let ttd = TempTedgeDir::new();
        let (mqtt, _http, _fs, _timer, _dl) = spawn_c8y_mapper_actor(&ttd, true).await;
        let mut mqtt = mqtt.with_timeout(TEST_TIMEOUT_MS);

        skip_init_messages(&mut mqtt).await;

        // Simulate a symlink exists
        ttd.dir("file-transfer")
            .dir("test-device")
            .dir("config_update")
            .file("path:type:A-1234");
        assert!(ttd
            .path()
            .join("file-transfer/test-device/config_update/path:type:A-1234")
            .exists());

        // Simulate config_update command with "executing" state
        mqtt.send(MqttMessage::new(
            &Topic::new_unchecked("te/device/main///cmd/config_update/1234"),
            json!({
            "status": "successful",
            "tedgeUrl": "http://localhost:8888/tedge/file-transfer/test-device/config_update/path:type:A-1234",
            "remoteUrl": "http://www.my.url",
            "type": "path/type/A",
        })
                .to_string(),
        ))
            .await
            .expect("Send failed");

        // Expect `503` smartrest message on `c8y/s/us`.
        assert_received_contains_str(&mut mqtt, [("c8y/s/us", "503,c8y_DownloadConfigFile")]).await;

        // Assert symlink is removed
        assert!(!ttd
            .path()
            .join("file-transfer/test-device/config_update/path:type:A-1234")
            .exists());
    }

    #[tokio::test]
    async fn handle_config_update_successful_cmd_for_child_device() {
        let ttd = TempTedgeDir::new();
        let (mqtt, _http, _fs, _timer, _dl) = spawn_c8y_mapper_actor(&ttd, true).await;
        let mut mqtt = mqtt.with_timeout(TEST_TIMEOUT_MS);

        skip_init_messages(&mut mqtt).await;

        // The child device must be registered first
        mqtt.send(MqttMessage::new(
            &Topic::new_unchecked("te/device/child1//"),
            r#"{ "@type":"child-device", "@id":"child1" }"#,
        ))
        .await
        .expect("fail to register the child-device");

        mqtt.skip(2).await; // Skip child device registration messages

        // Simulate config_update command with "executing" state
        mqtt.send(MqttMessage::new(
            &Topic::new_unchecked("te/device/child1///cmd/config_update/1234"),
            json!({
            "status": "successful",
            "tedgeUrl": "http://localhost:8888/tedge/file-transfer/child1/config_update/typeA-1234",
            "remoteUrl": "http://www.my.url",
            "type": "typeA",
        })
                .to_string(),
        ))
            .await
            .expect("Send failed");

        // Expect `503` smartrest message on child topic.
        assert_received_contains_str(
            &mut mqtt,
            [("c8y/s/us/child1", "503,c8y_DownloadConfigFile")],
        )
        .await;
    }
}

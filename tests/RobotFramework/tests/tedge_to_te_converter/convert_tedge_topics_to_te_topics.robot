*** Settings ***
Documentation       Purpose of this test is to verify that tedge-agent converts the tedge/# topics to te/# topics

Resource            ../../resources/common.resource
Library             ThinEdgeIO
Library             JSONLibrary

Suite Setup         Custom Setup
Suite Teardown      Custom Teardown

Test Tags           theme:mqtt    theme:tedge to te

*** Test Cases ***
Convert main device measurement topic
    Execute Command    tedge mqtt pub tedge/measurements ''
    ${messages_tedge}=    Should Have MQTT Messages    tedge/measurements    minimum=1    maximum=1
    ${messages_te}=    Should Have MQTT Messages    te/device/main///m/    minimum=1    maximum=1
    Should Be Equal    ${messages_tedge}  ${messages_te}    

Convert main device empty measurement topic
    Execute Command    tedge mqtt pub tedge/measurements '{"temperature":25}'
    ${messages_tedge}=    Should Have MQTT Messages    tedge/measurements    minimum=1    maximum=1
    ${messages_te}=    Should Have MQTT Messages    te/device/main///m/    minimum=1    maximum=1
    Should Be Equal    ${messages_tedge}  ${messages_te}
    Should Have MQTT Messages    te/device/main///m/    message_pattern={"temperature":25}
   
Convert child device measurement topic
    Execute Command    tedge mqtt pub tedge/measurements/child '{"temperature":25}'
    ${messages_tedge}=    Should Have MQTT Messages    tedge/measurements/child   minimum=1    maximum=1
    ${messages_te}=    Should Have MQTT Messages    te/device/child///m/    minimum=1    maximum=1
    Should Be Equal    ${messages_tedge}  ${messages_te}
    Should Have MQTT Messages    te/device/child///m/    message_pattern={"temperature":25}

Convert main device event topic
    Execute Command    tedge mqtt pub tedge/events/login_event '{"text":"someone logedin"}'
    ${messages_tedge}=    Should Have MQTT Messages    tedge/events/login_event   minimum=1    maximum=1
    ${messages_te}=    Should Have MQTT Messages    te/device/main///e/login_event    minimum=1    maximum=1
    Should Be Equal    ${messages_tedge}  ${messages_te}
    Should Have MQTT Messages    te/device/main///e/login_event    message_pattern={"text":"someone logedin"}

Convert main device empty event topic
    Execute Command    tedge mqtt pub tedge/events/login_event ''
    ${messages_tedge}=    Should Have MQTT Messages    tedge/events/login_event   minimum=1    maximum=1
    ${messages_te}=    Should Have MQTT Messages    te/device/main///e/login_event    minimum=1    maximum=1
    Should Be Equal    ${messages_tedge}  ${messages_te}      

Convert child device event topic
    Execute Command    tedge mqtt pub tedge/events/login_event/child '{"text":"someone logedin"}'
    ${messages_tedge}=    Should Have MQTT Messages    tedge/events/login_event/child   minimum=1    maximum=1
    ${messages_te}=    Should Have MQTT Messages    te/device/child///e/login_event    minimum=1    maximum=1
    Should Be Equal    ${messages_tedge}  ${messages_te}
    Should Have MQTT Messages    te/device/child///e/login_event    message_pattern={"text":"someone logedin"}

Convert main device alarm topic
    Execute Command    tedge mqtt pub tedge/alarms/minor/test_alarm '{"text":"test alarm"}' -q 2 -r
    ${messages}=    Should Have MQTT Messages    te/device/main///a/test_alarm    minimum=1    maximum=1
    ${message}=    Convert String To Json    ${messages[0]}
    Should Be Equal    ${message["severity"]}    minor

Convert main device alarm topic and retain
    Execute Command    tedge mqtt pub tedge/alarms/minor/test_alarm '{"text":"test alarm"}' -q 2 -r
    ${messages}=    Should Have MQTT Messages    te/device/main///a/test_alarm    minimum=1     maximum=1
    ${message}=    Convert String To Json    ${messages[0]}
    Should Be Equal    ${message["severity"]}    minor
    # Check if the retained message received with new client or not
    ${result}=    Execute Command    tedge mqtt sub te/device/main///a/test_alarm & sleep 2s; kill $!   
    Should Contain    ${result}    "severity":"minor"

Convert child device alarm topic
    Execute Command    tedge mqtt pub tedge/alarms/major/test_alarm/child '{"text":"test alarm"}' -q 2 -r
    ${messages}=    Should Have MQTT Messages    te/device/child///a/test_alarm    minimum=1     maximum=1
    ${message}=    Convert String To Json    ${messages[0]}
    Should Be Equal    ${message["severity"]}    major


Convert clear alarm topic
    Execute Command    tedge mqtt pub tedge/alarms/major/test_alarm/child '' -q 2 -r
    ${messages_tedge}=    Should Have MQTT Messages    tedge/alarms/major/test_alarm/child    minimum=1    maximum=1
    ${messages_te}=    Should Have MQTT Messages    te/device/child///a/test_alarm    minimum=1    maximum=1
    Should Be Equal    ${messages_tedge}  ${messages_te}
    
Convert empty alarm message
    Execute Command    tedge mqtt pub tedge/alarms/major/test_alarm/child {} -q 2 -r
    ${messages}=    Should Have MQTT Messages    te/device/child///a/test_alarm    minimum=1     maximum=1
    ${message}=    Convert String To Json    ${messages[0]}
    Should Be Equal    ${message["severity"]}    major

*** Keywords ***
Custom Setup
    Setup
    ThinEdgeIO.Service Health Status Should Be Up    tedge-agent

Custom Teardown
    Get Logs

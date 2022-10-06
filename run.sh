#!/bin/bash

comment=

# shellcheck disable=SC2086
sudo target/debug/container-ci-hotplug \
    run \
    -d parent-of:usb:2b3e:c310 \
    ${comment# NewAE ChipWhisperer CW310 } \
    -l usb:2b3e:c310:1=/dev/ttyACM_CW310_0 \
    -l usb:2b3e:c310:3=/dev/ttyACM_CW310_1 \
    ${comment# Olimex ARM-USB-TINY-H JTAG } \
    -l usb:15ba:002a:1=/dev/ttyUSB_JTAG_0 \
    ${comment# OpenTitan USB device } \
    -l usb:18d1:503a:0=/dev/ttyUSB_OT_0 \
    -l usb:18d1:503a:1=/dev/ttyUSB_OT_1 \
    -- \
    ${comment# Docker arguments } \
    -it \
    --name hotplug-cw310 \
    -v "$PWD"/ott:/ott \
    ubuntu:22.04 \
    /ott/start.sh

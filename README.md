# container-hotplug

Hot-plug (and unplug) devices into a Docker container as they are (un)plugged.

## Description

Docker provides the `--device` flag to give a container access to a device.
However the devices specified this way must be present when the container is created.

For dynamically created devices Docker provides the `--device-cgroup-rule`.
However this requires knowing the device's major and minor numbers, which are dynamically allocated by the kernel.
The rule accepts a glob `*` to mean "any minor" or "any major".
However this would still give the container access to all the devices handled by a particular driver.

This program tries to solve that problem by listening to udev events to detect when a device is (un)plugged.
It then interfaces directly with the container's cgroup to grant it access to that specific device.

To limit the devices the container can access, a _root device_ is specified.
The container will receive access to any device descending from the root device.
This is particularly useful if the root device is set to a USB hub.
However, since hubs are rarely interesting, it can be specified as "the parent of device X",
e.g., we can giving a container access to all devices connected to the same hub as an Arduino board.

Another concern is providing a container with well known paths for the devices.
On bare-metal systems this would usually be achieved with a `SYMLINK` directive in a udev rule.
This program tries to provide a similar functionality for containers, allowing you to specify symlinks for certain devices.

## Example

Give a container access to all devices connected to the same hub as a CW310 board.

1. Find the USB VID and PID of the device using `lsusb`, for a CW310 that is `2b3e:c310`
2. Run (as root) the container using `container-hotplug`:
```
container-hotplug run \
    -d parent-of:usb:2b3e:c310 \
    -- -it ubuntu:22.04 bash
```

If you want symlinks to the `tty` devices created by interfaces 1 and 3 of the CW310, run:
```
container-hotplug run \
    -d parent-of:usb:2b3e:c310 \
    -l usb:2b3e:c310:1=/dev/ttyACM_CW310_0 \
    -l usb:2b3e:c310:3=/dev/ttyACM_CW310_1 \
    -- -it ubuntu:22.04 bash
```
# container-hotplug

Hot-plug (and unplug) devices into a container as they are (un)plugged.

## Description

Docker provides the `--device` flag to give a container access to a device.
However the devices specified this way must be present when the container is created.

For dynamically created devices Docker provides the `--device-cgroup-rule`.
However this requires knowing the device's major and minor numbers, which are dynamically allocated by the kernel.
The rule accepts a glob `*` to mean "any minor" or "any major".
However this would still give the container access to all the devices handled by a particular driver.

This program tries to solve that problem by listening to udev events to detect when a device is (un)plugged.
It then interfaces directly with the container's cgroup to grant it access to that specific device.

To limit the devices the container can access, _root devices_ are specified.
The container will receive access to any device descending from one of the root devices.
This is particularly useful if the root device is set to a USB hub.
The hub can be specified directly, or it can be specified as "the parent of device X",
e.g., we can giving a container access to all devices connected to the same hub as an Arduino board.

Another concern is providing a container with well known paths for the devices.
On bare-metal systems this would usually be achieved with a `SYMLINK` directive in a udev rule.
This program tries to provide a similar functionality for containers, allowing you to specify symlinks for certain devices.

## Usage

This tool wraps `runc` with the additional hotplug feature, therefore it can be used as a drop in replace for
many container managers/orchestrators such as Docker, Podman, and Kubernetes. You need to ensure `runc` is available in your `PATH`
so `container-hotplug` can find it.

It supports two annotations, `org.lowrisc.hotplug.devices` and `org.lowrisc.hotplug.symlinks`.

For Docker, you can specify an alternative runtime by [changing /etc/docker/daemon.json](https://docs.docker.com/engine/alternative-runtimes/#youki):
```json
{
  "runtimes": {
    "hotplug": {
      "path": "/path/to/container-hotplug/binary"
    }
  }
}
```
and use it with the `--runtime hotplug` flag and appropriate annotation, e.g.
```bash
sudo docker run --runtime hotplug -it --annotation org.lowrisc.hotplug.devices=parent-of:usb:2b2e:c310 ubuntu:latest
```

For podman, you can specify the path directly, by:
```bash
sudo podman run --runtime /path/to/container-hotplug/binary -it --annotation org.lowrisc.hotplug.devices=parent-of:usb:2b2e:c310 ubuntu:latest
```

For containerd (e.g. when using kubernetes), you can edit `/etc/containerd/config.toml` to add:
```toml
[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.hotplug]
  runtime_type = "io.containerd.runc.v2"
  pod_annotations = ["org.lowrisc.hotplug.*"]

[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.hotplug.options]
  SystemdCgroup = true
  BinaryName = "/path/to/container-hotplug/binary"
```
this would allow you to use `hotplug` as handler in k8s, e.g. add a runtime class with
```yaml
apiVersion: node.k8s.io/v1
kind: RuntimeClass
metadata:
  name: hotplug
handler: hotplug
```
and use it in a pod with
```yaml
apiVersion: v1
kind: Pod
metadata:
  name: ubuntu
  annotations:
    org.lowrisc.hotplug.devices: parent-of:usb:0bda:5634
spec:
  runtimeClassName: hotplug
  containers:
  - name: ubuntu
    image: ubuntu:latest
    stdin: true
    tty: true
```

If you want symlinks to the `tty` devices created by interfaces 1 and 3 of the CW310, add
```
--annotation org.lowrisc.hotplug.symlinks=usb:2b3e:c310:1=/dev/ttyACM_CW310_0,usb:2b3e:c310:3=/dev/ttyACM_CW310_1
```
to docker/podman command line or
```
org.lowrisc.hotplug.symlinks: usb:2b3e:c310:1=/dev/ttyACM_CW310_0,usb:2b3e:c310:3=/dev/ttyACM_CW310_1
```
to k8s config.

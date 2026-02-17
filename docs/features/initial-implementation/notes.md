Here is the complete architectural summary of the **Podman-based Secure Sandbox Environment** we have built. You can pass this directly to another agent or engineer to replicate the setup.

### 1. High-Level Architecture

We transitioned from `docker-compose` to **Kubernetes-style Pods** running natively in Podman. This eliminates race conditions and provides strict network isolation.

* **Network Strategy:** A dedicated bridge network (`secure_net`) where Pods can resolve each other by name, but cannot reach the internet directly.
* **Security Layer 1 (NFTables):** "Jailer" sidecars (`*_node`) run with `NET_ADMIN` caps. They use `nftables` to block all outbound traffic *except* DNS and traffic destined for the Proxy.
* **Security Layer 2 (HAProxy):** A central Proxy Pod that enforces Access Control Lists (ACLs). It identifies clients by Source IP and filters destinations using allow-lists.

---

### 2. File Structure

The project relies on these files on the host:

```text
.
├── pods.yaml                 # The K8s-style definition for all 3 pods
├── scripts/
│   ├── jail_cli.sh           # NFTables script for CLI Pod
│   └── jail_sandbox.sh       # NFTables script for Sandbox Pod
└── haproxy/
    ├── haproxy.cfg           # Template config (uses placeholders for IPs)
    ├── startup.sh            # Boot script to resolve IPs and start HAProxy
    ├── cli_domains.lst       # Allow-list for CLI (e.g., googleapis.com)
    └── sandbox_domains.lst   # Allow-list for Sandbox (e.g., python.org)

```

---

### 3. Critical Configuration Files

#### A. `pods.yaml` (Infrastructure)

Defines 3 Pods. Note the `hostPath` placeholders which are replaced at runtime.

```yaml
apiVersion: v1
kind: Pod
metadata: { name: proxy-pod, labels: { app: proxy } }
spec:
  containers:
  - name: proxy
    image: docker.io/library/haproxy:alpine
    command: ["/bin/sh", "/opt/haproxy/startup.sh"]
    volumeMounts: [{ name: haproxy-dir, mountPath: /opt/haproxy }]
    ports: [{ containerPort: 8080, hostPort: 5050 }]
  volumes: [{ name: haproxy-dir, hostPath: { path: PROJECT_ROOT/haproxy, type: Directory } }]
---
apiVersion: v1
kind: Pod
metadata: { name: sandbox-pod, labels: { app: sandbox } }
spec:
  containers:
  - name: sandbox-node
    image: alpine:latest
    securityContext: { capabilities: { add: ["NET_ADMIN"] } }
    command: ["/bin/sh", "/opt/scripts/jail_sandbox.sh"]
    volumeMounts: [{ name: scripts-dir, mountPath: /opt/scripts }]
  - name: sandbox-app
    image: python:3.9-slim
    command: ["sleep", "infinity"]
  volumes: [{ name: scripts-dir, hostPath: { path: PROJECT_ROOT/scripts, type: Directory } }]
---
apiVersion: v1
kind: Pod
metadata: { name: cli-pod, labels: { app: cli } }
spec:
  containers:
  - name: cli-node
    image: alpine:latest
    securityContext: { capabilities: { add: ["NET_ADMIN"] } }
    command: ["/bin/sh", "/opt/scripts/jail_cli.sh"]
    volumeMounts: [{ name: scripts-dir, mountPath: /opt/scripts }]
  - name: cli-app
    image: curlimages/curl:latest
    command: ["sleep", "infinity"]
    env:
    - { name: http_proxy, value: "http://proxy-pod:8080" }
    - { name: https_proxy, value: "http://proxy-pod:8080" }
  volumes: [{ name: scripts-dir, hostPath: { path: PROJECT_ROOT/scripts, type: Directory } }]

```

#### B. `haproxy/startup.sh` (Dynamic Configuration)

Solves the "Static IP" problem by resolving Pod IPs at boot time.

```bash
#!/bin/sh
set -e
# 1. Wait for Peers
CLI_IP=""
SANDBOX_IP=""
while [ -z "$CLI_IP" ] || [ -z "$SANDBOX_IP" ]; do
    echo "Resolving peers..."
    CLI_IP=$(getent hosts cli-pod | awk '{ print $1 }' | head -n 1)
    SANDBOX_IP=$(getent hosts sandbox-pod | awk '{ print $1 }' | head -n 1)
    [ -z "$CLI_IP" ] || [ -z "$SANDBOX_IP" ] && sleep 2
done

# 2. Inject IPs into Config
cp /opt/haproxy/haproxy.cfg /tmp/haproxy_generated.cfg
sed -i "s/REPLACE_CLI_IP/$CLI_IP/g" /tmp/haproxy_generated.cfg
sed -i "s/REPLACE_SANDBOX_IP/$SANDBOX_IP/g" /tmp/haproxy_generated.cfg

# 3. Start HAProxy
exec haproxy -f /tmp/haproxy_generated.cfg

```

#### C. `haproxy/haproxy.cfg` (The Rules Engine)

Uses external files for domain lists and placeholders for source IPs.

```haproxy
global
    log stdout format raw local0
defaults
    mode http
    timeout connect 5s
    timeout client 1m
    timeout server 1m

resolvers podman_dns
    nameserver dns1 10.89.0.1:53

frontend https_proxy
    bind *:8080
    
    # Access Control Lists
    acl src_is_cli     src -i REPLACE_CLI_IP
    acl src_is_sandbox src -i REPLACE_SANDBOX_IP
    
    # Domain Allow-Lists (External Files)
    acl dest_cli_allowed     hdr(host) -m end -i -f /opt/haproxy/cli_domains.lst
    acl dest_sandbox_allowed hdr(host) -m end -i -f /opt/haproxy/sandbox_domains.lst

    # Logic
    http-request deny if src_is_cli !dest_cli_allowed
    http-request deny if src_is_sandbox !dest_sandbox_allowed
    http-request deny if !src_is_cli !src_is_sandbox

    default_backend dynamic_internet

backend dynamic_internet
    http-request do-resolve(txn.dst,podman_dns,ipv4) hdr(host)
    http-request set-dst var(txn.dst)
    server clear 0.0.0.0:0

```

---

### 4. How to Operate

**Start the System:**

```bash
# 1. Ensure Network Exists
podman network create secure_net

# 2. Deploy (Injects current path into YAML)
sed "s|PROJECT_ROOT|$(pwd)|g" pods.yaml | podman play kube --network secure_net -

```

**Update Allow-Lists (Zero Downtime):**

1. Edit `haproxy/sandbox_domains.lst` on the host.
2. Run reload:
```bash
podman exec proxy-pod-proxy kill -USR2 1

```



**Verify Security:**

```bash
# Should SUCCEED (Allowed in cli_domains.lst)
podman exec -it cli-pod-cli-app curl -v https://storage.googleapis.com

# Should FAIL (Blocked by Proxy)
podman exec -it cli-pod-cli-app curl -v https://example.com

# Should FAIL (Blocked by NFTables Jailer)
podman exec -it cli-pod-cli-app curl --noproxy "*" -v http://1.1.1.1

```

//! The browser pod's manifests say what its fence depends on.
//!
//! What the fence does is proved on the cluster after every deploy
//! (`scripts/check-browser-fence.sh`). These read the files as Kubernetes
//! will, so that an edit which would open the fence fails here first. The
//! fence's rules are compared whole: one peer more in any of them, an
//! empty selector, a rule without its `to:`, is a rule that lets in or out
//! more than it says, and a mounted secret or a token hands a page that
//! took the pod's Chrome over what the pod was made to keep from it.

use serde::Deserialize;
use serde_yaml_ng::Value;
use std::path::PathBuf;

fn read(path: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// `browser.yaml` as the deploy hands it to the cluster, with the node's
/// addresses given as `node`, or why it is no YAML.
fn parsed(node: &str) -> Result<Vec<Value>, String> {
    let text = read("deploy/k8s/browser.yaml").replace("${SCOUT_SHA}", "abc1234").replace("${SCOUT_NODE_EXCEPT}", node);
    serde_yaml_ng::Deserializer::from_str(&text).map(|doc| Value::deserialize(doc).map_err(|e| e.to_string())).collect()
}

fn document(kind: &str) -> Value {
    parsed("203.0.113.4/32")
        .unwrap()
        .into_iter()
        .find(|doc| doc["kind"] == kind)
        .unwrap_or_else(|| panic!("browser.yaml has no {kind}"))
}

fn yaml(text: &str) -> Value {
    serde_yaml_ng::from_str(text).unwrap()
}

#[test]
fn the_browser_pod_is_these_three_things_and_no_others() {
    // Policies add up: a second one that selects the pod, or one for the
    // whole namespace, lets through whatever it lets through, beside the
    // fence. And a second Deployment with the pod's label would stand
    // behind its Service, holding whatever it was given.
    let kinds: Vec<(String, String)> = parsed("203.0.113.4/32")
        .unwrap()
        .iter()
        .map(|doc| (doc["kind"].as_str().unwrap_or_default().to_string(), doc["metadata"]["name"].as_str().unwrap_or_default().to_string()))
        .collect();
    let expected = [("Deployment", "scout-browser"), ("Service", "scout-browser"), ("NetworkPolicy", "scout-browser")];
    assert_eq!(kinds, expected.map(|(kind, name)| (kind.to_string(), name.to_string())));
    let manifests = std::fs::read_dir(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/k8s")).unwrap();
    for manifest in manifests {
        let path = manifest.unwrap().path();
        if path.file_name().is_some_and(|name| name == "browser.yaml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("NetworkPolicy"), "{} holds a NetworkPolicy, and it adds to the fence", path.display());
        assert!(!text.contains("app: scout-browser"), "{} names the browser pod's label", path.display());
    }
}

#[test]
fn the_fence_is_these_rules_and_no_others() {
    let policy = document("NetworkPolicy");
    assert_eq!(policy["metadata"]["namespace"], "scout");
    let expected = yaml(
        "
        podSelector: { matchLabels: { app: scout-browser } }
        policyTypes: [Ingress, Egress]
        ingress:
          # Scout's pod, on the one port.
          - from: [{ podSelector: { matchLabels: { app: scout } } }]
            ports: [{ protocol: TCP, port: 8091 }]
        egress:
          # CoreDNS, one peer: its namespace AND its pods. Two list items
          # would be either, and the namespace alone every pod in it.
          - to:
              - namespaceSelector: { matchLabels: { kubernetes.io/metadata.name: kube-system } }
                podSelector: { matchLabels: { k8s-app: kube-dns } }
            ports: [{ protocol: UDP, port: 53 }, { protocol: TCP, port: 53 }]
          # The public internet, TCP alone.
          - to:
              - ipBlock:
                  cidr: 0.0.0.0/0
                  except: [203.0.113.4/32, 0.0.0.0/8, 10.0.0.0/8, 100.64.0.0/10, 127.0.0.0/8, 169.254.0.0/16,
                           172.16.0.0/12, 192.168.0.0/16, 224.0.0.0/4, 240.0.0.0/4]
            ports: [{ protocol: TCP }]
        ",
    );
    assert_eq!(policy["spec"], expected, "the fence is not the one these tests know");
}

#[test]
fn the_node_is_excepted_or_the_manifest_does_not_apply() {
    // Every address the node has, however many.
    let policy = parsed("203.0.113.4/32, 198.51.100.7/32, 10.42.0.1/32").unwrap().into_iter().find(|doc| doc["kind"] == "NetworkPolicy").unwrap();
    let except = &policy["spec"]["egress"][1]["to"][0]["ipBlock"]["except"];
    for node in ["203.0.113.4/32", "198.51.100.7/32", "10.42.0.1/32"] {
        assert!(except.as_sequence().unwrap().iter().any(|range| range == node), "{node}: {except:?}");
    }
    // None at all: no YAML, so the apply fails rather than leaving them out.
    assert!(parsed("").is_err(), "an empty list of the node's addresses still made a manifest");
    let deploy = read("scripts/deploy-k3s.sh");
    assert!(deploy.contains(r#": "${SCOUT_NODE_EXCEPT:?"#), "the deploy goes ahead without the node's addresses");
}

#[test]
fn the_browser_pod_holds_no_secret_no_token_and_cannot_write_its_root() {
    let deployment = document("Deployment");
    assert_eq!(deployment["metadata"]["namespace"], "scout");
    let template = &deployment["spec"]["template"];
    // Its own label alone: `app: scout` would put it behind Scout's
    // Service and inside its own fence's way in.
    assert_eq!(template["metadata"]["labels"], yaml("{ app: scout-browser }"));
    assert_eq!(deployment["spec"]["selector"], yaml("{ matchLabels: { app: scout-browser } }"));

    let pod = &template["spec"];
    assert_eq!(pod["automountServiceAccountToken"], false);
    assert_eq!(pod["enableServiceLinks"], false);
    assert_eq!(pod["securityContext"], yaml("{ runAsNonRoot: true, runAsUser: 1000, runAsGroup: 1000, fsGroup: 1000, seccompProfile: { type: RuntimeDefault } }"));
    for absent in [
        "serviceAccountName",
        "serviceAccount",
        "hostNetwork",
        "hostPID",
        "hostIPC",
        "shareProcessNamespace",
        "initContainers",
        "ephemeralContainers",
        "imagePullSecrets",
    ] {
        assert!(pod.get(absent).is_none(), "the browser pod has {absent}");
    }
    // Scratch only: no secret, no projected token, no host path, no claim.
    let volumes = pod["volumes"].as_sequence().unwrap();
    for volume in volumes {
        let keys: Vec<&str> = volume.as_mapping().unwrap().keys().filter_map(Value::as_str).collect();
        assert_eq!(keys, ["name", "emptyDir"], "{volume:?}");
    }

    let containers = pod["containers"].as_sequence().unwrap();
    assert_eq!(containers.len(), 1, "one container, held to all of this");
    let browser = &containers[0];
    assert_eq!(browser["image"], "scout:abc1234");
    // The image's entrypoint is tini, which reaps what Chrome leaves; a
    // `command` would replace it.
    assert_eq!(browser["args"], yaml("[scout-browser]"));
    assert!(browser.get("command").is_none() && browser.get("envFrom").is_none());
    assert_eq!(browser["securityContext"], yaml("{ allowPrivilegeEscalation: false, readOnlyRootFilesystem: true, capabilities: { drop: [ALL] } }"));
    // Each variable a value written here: none from a secret or the API.
    let env = browser["env"].as_sequence().unwrap();
    for variable in env {
        let keys: Vec<&str> = variable.as_mapping().unwrap().keys().filter_map(Value::as_str).collect();
        assert_eq!(keys, ["name", "value"], "{variable:?}");
    }
    let named = |name: &str| env.iter().find(|v| v["name"] == name).map(|v| v["value"].clone());
    assert_eq!(named("SCOUT_BROWSER_BIND"), Some(Value::from("0.0.0.0:8091")));
    assert!(named("SCOUT_BROWSER_FENCE").and_then(|v| v.as_str().map(str::to_string)).is_some_and(|fence| !fence.trim().is_empty()));
    // A port of the pod's network, not the node's.
    assert_eq!(browser["ports"], yaml("[{ name: browser, containerPort: 8091 }]"));
    assert_eq!(browser["volumeMounts"], yaml("[{ name: tmp, mountPath: /tmp }, { name: home, mountPath: /home/scout }]"));
    assert_eq!(browser["resources"]["limits"], yaml("{ memory: 1536Mi }"));
}

#[test]
fn scout_asks_the_pod_by_its_service_and_nothing_else_reaches_it() {
    let service = document("Service");
    // A ClusterIP of the cluster's own: no node port, no load balancer.
    assert_eq!(service["spec"], yaml("{ selector: { app: scout-browser }, ports: [{ name: browser, port: 8091, targetPort: browser }] }"));
    let scout = read("deploy/k8s/deployment.yaml");
    assert!(scout.contains(r#"{ name: SCOUT_BROWSER_URL, value: "http://scout-browser:8091" }"#), "Scout's pod does not name the browser pod");
}

#[test]
fn the_names_the_deploy_fills_in_are_the_two_it_is_told_to() {
    // envsubst with a list substitutes those names and leaves anything
    // else as written, so a third would reach the cluster as `${…}`.
    let manifest = read("deploy/k8s/browser.yaml");
    let mut names: Vec<&str> = manifest.split("${").skip(1).filter_map(|rest| rest.split_once('}').map(|(name, _)| name)).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names, ["SCOUT_NODE_EXCEPT", "SCOUT_SHA"]);
    let deploy = read("scripts/deploy-k3s.sh");
    assert!(deploy.contains(r#"envsubst '${SCOUT_SHA} ${SCOUT_NODE_EXCEPT}' < deploy/k8s/browser.yaml"#), "the deploy names other variables");
}

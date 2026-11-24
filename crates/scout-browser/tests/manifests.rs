//! The browser pod's manifests say what its fence depends on.
//!
//! What the fence does is proved on the cluster after every deploy
//! (`scripts/check-browser-fence.sh`). These read the files, so that an
//! edit which would open it fails here first: an egress rule left without
//! its `to:` allows everything, and a secret mounted into the pod hands a
//! page that took its Chrome over what the pod was made to keep from it.

use std::path::PathBuf;

fn read(path: &str) -> String {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    std::fs::read_to_string(root.join(path)).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// The documents of `browser.yaml`, by kind.
fn document(kind: &str) -> String {
    read("deploy/k8s/browser.yaml")
        .split("\n---\n")
        .find(|doc| doc.lines().any(|line| line == format!("kind: {kind}")))
        .unwrap_or_else(|| panic!("browser.yaml has no {kind}"))
        .to_string()
}

/// The lines that are not comments: a rule written in a comment is no rule.
fn said(doc: &str) -> Vec<&str> {
    doc.lines().filter(|line| !line.trim_start().starts_with('#')).collect()
}

#[test]
fn the_browser_pod_holds_no_secret_no_token_and_cannot_write_its_root() {
    let pod = document("Deployment");
    let lines = said(&pod).join("\n");
    for absent in ["envFrom", "secretRef", "secretKeyRef", "serviceAccountName", "hostNetwork", "hostPath", "privileged"] {
        assert!(!lines.contains(absent), "the browser pod names {absent}");
    }
    for present in [
        "automountServiceAccountToken: false",
        "enableServiceLinks: false",
        "runAsNonRoot: true",
        "allowPrivilegeEscalation: false",
        "readOnlyRootFilesystem: true",
        "capabilities: { drop: [ALL] }",
        "seccompProfile: { type: RuntimeDefault }",
        "limits: { memory: 1536Mi }",
    ] {
        assert!(lines.contains(present), "the browser pod lacks {present}");
    }
    // The image's entrypoint is tini, which reaps what Chrome leaves; a
    // `command` would replace it.
    assert!(lines.contains(r#"args: ["scout-browser"]"#) && !lines.contains("command:"));
    // Told what fences it, or it would not listen beyond loopback.
    assert!(lines.contains("SCOUT_BROWSER_FENCE") && lines.contains(r#"value: "0.0.0.0:8091""#));
}

#[test]
fn the_fence_lets_in_scout_alone() {
    let policy = document("NetworkPolicy");
    let lines = said(&policy);
    let ingress: Vec<&str> = lines.iter().skip_while(|line| line.trim() != "ingress:").take_while(|line| line.trim() != "egress:").copied().collect();
    let from: Vec<&&str> = ingress.iter().filter(|line| line.trim().starts_with("- from:")).collect();
    assert_eq!(from.len(), 1, "one way in: {ingress:#?}");
    assert!(ingress.iter().any(|line| line.trim() == "matchLabels: { app: scout }"), "{ingress:#?}");
    assert!(ingress.iter().any(|line| line.trim() == "- { protocol: TCP, port: 8091 }"), "{ingress:#?}");
    assert!(!ingress.iter().any(|line| line.contains("ipBlock") || line.contains("namespaceSelector")), "{ingress:#?}");
    assert!(lines.iter().any(|line| line.trim() == "policyTypes: [Ingress, Egress]"));
}

#[test]
fn the_fence_lets_out_dns_and_the_public_internet_over_tcp_and_nothing_else() {
    let policy = document("NetworkPolicy");
    let lines = said(&policy);
    let egress: Vec<&str> = lines.iter().skip_while(|line| line.trim() != "egress:").skip(1).copied().collect();
    // Every rule has its `to:`: written without one, a rule allows
    // everything (measured on this cluster, 2026-11-20).
    let indent = egress[0].len() - egress[0].trim_start().len();
    let rules: Vec<&&str> = egress.iter().filter(|line| line.len() - line.trim_start().len() == indent && line.trim_start().starts_with("- ")).collect();
    assert_eq!(rules.len(), 2, "two ways out: {egress:#?}");
    assert!(rules.iter().all(|rule| rule.trim() == "- to:"), "a rule without its to: {rules:?}");

    let text = egress.join("\n");
    let (dns, public) = text.split_once("ipBlock").expect("a public rule");
    assert!(dns.contains("k8s-app: kube-dns") && dns.contains("kubernetes.io/metadata.name: kube-system"), "{dns}");
    assert!(dns.contains("{ protocol: UDP, port: 53 }") && dns.contains("{ protocol: TCP, port: 53 }"), "{dns}");
    assert!(public.contains("cidr: 0.0.0.0/0"), "{public}");
    // TCP alone, and on any port: no datagram leaves for the internet.
    assert!(public.contains("- { protocol: TCP }") && !public.contains("UDP") && !public.contains("SCTP"), "{public}");
    for excepted in [
        "0.0.0.0/8",
        "10.0.0.0/8",
        "100.64.0.0/10",
        "127.0.0.0/8",
        "169.254.0.0/16",
        "172.16.0.0/12",
        "192.168.0.0/16",
        "224.0.0.0/4",
        "240.0.0.0/4",
        "${SCOUT_NODE_EXCEPT}",
    ] {
        assert!(public.contains(excepted), "{excepted} is not excepted: {public}");
    }
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

#[test]
fn scout_asks_the_pod_by_its_service() {
    let service = document("Service");
    assert!(service.contains("name: scout-browser") && service.contains("{ name: browser, port: 8091, targetPort: browser }"), "{service}");
    let scout = read("deploy/k8s/deployment.yaml");
    assert!(scout.contains(r#"{ name: SCOUT_BROWSER_URL, value: "http://scout-browser:8091" }"#), "Scout's pod does not name the browser pod");
}

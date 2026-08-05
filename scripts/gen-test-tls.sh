#!/usr/bin/env bash
# Regenerate the conformance suite's committed test PKI
# (conformance/server/tls/): a throwaway CA and a leaf for the loopback
# echo server. TEST MATERIAL ONLY — the private keys are public by
# design; nothing outside loopback conformance runs must ever trust this
# CA.
#
# Committed rather than generated per run so trust provisioning that must
# happen before process start (NODE_EXTRA_CA_CERTS is read at Node
# startup) can reference a stable path.
set -euo pipefail
cd "$(dirname "$0")/../conformance/server/tls"

days=36500
openssl ecparam -name prime256v1 -genkey -noout -out ca.key.pem
openssl req -x509 -new -key ca.key.pem -sha256 -days "$days" \
    -subj "/CN=lann-websocket conformance TEST CA (do not trust)" \
    -addext "basicConstraints=critical,CA:TRUE" \
    -addext "keyUsage=critical,keyCertSign" \
    -out ca.pem
openssl ecparam -name prime256v1 -genkey -noout -out leaf.key.pem
openssl req -new -key leaf.key.pem \
    -subj "/CN=conformance-echod TEST leaf" -out leaf.csr
openssl x509 -req -in leaf.csr -CA ca.pem -CAkey ca.key.pem \
    -CAcreateserial -sha256 -days "$days" \
    -extfile <(printf "subjectAltName=IP:127.0.0.1,DNS:localhost\nbasicConstraints=CA:FALSE\nkeyUsage=digitalSignature\nextendedKeyUsage=serverAuth\n") \
    -out leaf.pem
rm -f leaf.csr ca.srl ca.key.pem
echo "regenerated: ca.pem leaf.pem leaf.key.pem (ca.key.pem discarded)"

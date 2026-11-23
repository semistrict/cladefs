# The tests' peer credentials

Known to all, never a host's: the credentials the tests' daemons authenticate each other with
(`CLADEFSD_PEER_TLS`, board 8437), which `.cargo/config.toml` names for every process Cargo runs,
and a second set, `other/`, of another CA, which the hosts' refuse. Each is the CA's certificate
(`ca.crt`), a host's certificate for `cladefs-peer`, valid until 2126, for both server and client
authentication (`peer.crt`), and its key (`peer.key`), ECDSA P-256. The CAs' keys were not kept:
to make new ones, as these were made (LibreSSL 3.3):

```sh
openssl ecparam -name prime256v1 -genkey -noout -out ca.key
openssl req -x509 -new -key ca.key -sha256 -days 36500 -subj "/CN=cladefs tests' hosts CA" \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" -out ca.crt
# A named curve: LibreSSL's genpkey writes the curve's parameters out, which rustls refuses.
openssl ecparam -name prime256v1 -genkey -noout -out peer.sec1
openssl pkcs8 -topk8 -nocrypt -in peer.sec1 -out peer.key
openssl req -new -key peer.key -subj "/CN=cladefs-peer" -out peer.csr
printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth,clientAuth\nsubjectAltName=DNS:cladefs-peer\n' > ext
openssl x509 -req -in peer.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 36500 -sha256 -extfile ext -out peer.crt
```

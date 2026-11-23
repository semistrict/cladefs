# The tests' OTLP collector credentials

Known to all, never a host's: the certificate an OTLP collector of `tests/tracing.rs` serves HTTPS
with (board 8569), so that the daemon's exporter is shown to reach an `https://` endpoint, as
`SSL_CERT_FILE` naming `ca.crt` has it trust the collector. The CA's certificate (`ca.crt`), the
collector's certificate for `localhost` and `127.0.0.1`, valid until 2126, for server
authentication (`collector.crt`), and its key (`collector.key`), ECDSA P-256. The CA's key was not
kept: to make new ones, as these were made (LibreSSL 3.3):

```sh
openssl ecparam -name prime256v1 -genkey -noout -out ca.key
openssl req -x509 -new -key ca.key -sha256 -days 36500 -subj "/CN=cladefs tests' OTLP collector CA" \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" -out ca.crt
# A named curve: LibreSSL's genpkey writes the curve's parameters out, which rustls refuses.
openssl ecparam -name prime256v1 -genkey -noout -out collector.sec1
openssl pkcs8 -topk8 -nocrypt -in collector.sec1 -out collector.key
openssl req -new -key collector.key -subj "/CN=localhost" -out collector.csr
printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:localhost,IP:127.0.0.1\n' > ext
openssl x509 -req -in collector.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 36500 -sha256 -extfile ext -out collector.crt
```

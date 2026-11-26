# The tests' TLS credentials

Known to all, never a host's: what machined's tests serve TCP with (`server.crt`, for `machined`,
server authentication) and the client certificate their callers present (`client.crt`, client
authentication), both of the CA `ca.crt`, and `other/`, a client of another CA, which machined
refuses. ECDSA P-256, valid until 2126. The CAs' keys were not kept: to make new ones, as these were
made (LibreSSL 3.3), for each of `.` and `other`:

```sh
openssl ecparam -name prime256v1 -genkey -noout -out ca.key
openssl req -x509 -new -key ca.key -sha256 -days 36500 -subj "/CN=machined tests' CA" \
    -addext "basicConstraints=critical,CA:TRUE" -addext "keyUsage=critical,keyCertSign,cRLSign" -out ca.crt
# For server (serverAuth, DNS:machined) and client (clientAuth, DNS:machined-client):
openssl ecparam -name prime256v1 -genkey -noout -out server.sec1
openssl pkcs8 -topk8 -nocrypt -in server.sec1 -out server.key
openssl req -new -key server.key -subj "/CN=machined-server" -out server.csr
printf 'basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth\nsubjectAltName=DNS:machined\n' > ext
openssl x509 -req -in server.csr -CA ca.crt -CAkey ca.key -CAcreateserial -days 36500 -sha256 -extfile ext -out server.crt
```

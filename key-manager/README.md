# key-manager

A CLI tool for managing encryption keys used by AMPC parties. It generates ECDH key pairs and manages their storage in AWS S3 (public keys) and AWS Secrets Manager (private keys).

## Kubernetes Usage

Deploy a temporary pod - needs access to the kubernetes cluster, where AMPC is deployed:

```yaml
apiVersion: v1
kind: Pod
metadata:
  name: temporary-key-manager
  namespace: <your-namespace>
spec:
  serviceAccountName: <your-service-account>
  hostNetwork: true
  containers:
  - name: key-manager
    image: ghcr.io/worldcoin/ampc-key-manager:latest
    imagePullPolicy: Always
    command: ["/bin/bash"]
    args: ["-c", "while true; do ping localhost; sleep 60; done"]
  imagePullSecrets:
  - name: github-secret
```

Execute the key rotation command **twice**:

```bash
kubectl exec -n <your-namespace> -it temporary-key-manager -- key-manager \
  --node-id <0|1|2> \
  --env <environment> \
  --region <aws-region> \
  --app-name <app-name> \
  --public-key-bucket-name <bucket-name>
  rotate \
  --dry-run false
```

**Arguments:**
- `--node-id` (required): Node identifier, must be `0`, `1`, or `2`
- `--env` (default: `stage`): Environment name
- `--region` (default: `eu-north-1`): AWS region
- `--app-name` (default: `iris-mpc`): Application name used in secret IDs
- `--public-key-bucket-name`: S3 bucket name for public keys (optional, defaults to `wf-smpcv2-stage-public-keys`)
- `--dry-run`: If set, generates keys but doesn't upload them

Delete the temporary pod:

```bash
kubectl delete pod temporary-key-manager
```

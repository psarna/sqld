# `sqld` User Guide

## Deploying to Fly

You can use the existing `fly.toml` file from this repository.

Just run
```console
flyctl launch
```
... then pick a name and respond "Yes" when the prompt asks you to deploy.

Finally, allocate an IPv6 addres:
```
flyctl ips allocate-v6 -a <your app name>
```

You now have `sqld` running on Fly listening to port `5000`.

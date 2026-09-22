# Local Service Resources

Garden discovers packaged local services from the app resource directory.

The runtime lookup paths are:

```text
services/choreograph/service.json
services/kg-ultra/service.json
```

Keep `service.json` out of this directory until a matching service artifact is
also staged. A manifest without its command target makes Garden report the
service as configured but unable to start.

`service.example.json` documents each packaged-service contract. The checked-in
examples are not loaded at runtime.

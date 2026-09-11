# nightscout-waybar-module

A basic rust script to output [NightScout](http://www.nightscout.info/) data in JSON for use in a [Waybar](https://github.com/Alexays/Waybar/) module.

## Usage

```
Usage: nightscout-waybar-module [OPTIONS] <URL>

Arguments:
  <URL>  NightScout base URL (e.g. http://localhost:1337)

Options:
  -c, --config <CONFIG>  Path to config file (TOML)
      --mmol             Use mmol/L units (instead of mg/dL default)
  -h, --help             Print help
```

See the [default config values](config_defaults.toml) for a schema. Note that BG values for thresholds should be in mg/dL.

### Waybar snippet

Remember to replace the URL and config path.

```json
{
  "custom/nightscout": {
    "exec": "nightscout-waybar-module <URL> -c <CONFIG FILE PATH>",
    "return-type": "json",
    "interval": 300,
    "format": "{}",
    "on-click": "xdg-open <URL>"
  }
}
```

## License

This project is dual-licensed under both [MIT](LICENSE-MIT) and [Apache-2.0](LICENSE-APACHE).

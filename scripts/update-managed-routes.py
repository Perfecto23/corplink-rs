#!/usr/bin/env python3
import argparse
import hashlib
import ipaddress
import json
import os
import sys
import tempfile
import time
import urllib.parse
import urllib.request
from pathlib import Path


DEFAULT_CACHE_FILE = ".run/managed-routes-cache.json"
DEFAULT_GITHUB_META_URL = "https://api.github.com/meta"
DEFAULT_GITHUB_KEYS = ("web", "api", "git")
DEFAULT_DOH_URL = "https://cloudflare-dns.com/dns-query"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Resolve corplink-rs managed_routes without printing secrets."
    )
    parser.add_argument("config", type=Path, help="Path to corplink-rs config.json")
    parser.add_argument(
        "--write-cache",
        action="store_true",
        help="Write the resolved managed routes cache atomically.",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the resolved managed routes cache. This is the default.",
    )
    return parser.parse_args()


def load_json(path: Path) -> dict:
    with path.open("r", encoding="utf-8") as file:
        data = json.load(file)
    if not isinstance(data, dict):
        raise ValueError(f"{path} must contain a JSON object")
    return data


def normalize_route(value: str) -> str:
    value = value.strip()
    if not value:
        raise ValueError("route is empty")
    if "/" in value:
        return str(ipaddress.ip_network(value, strict=False))
    ip = ipaddress.ip_address(value)
    return f"{ip}/{32 if ip.version == 4 else 128}"


def dedupe(values: list[str]) -> list[str]:
    seen: set[str] = set()
    out: list[str] = []
    for value in values:
        if value not in seen:
            out.append(value)
            seen.add(value)
    return out


def fetch_github_meta(url: str) -> dict:
    request = urllib.request.Request(
        url,
        headers={
            "Accept": "application/vnd.github+json",
            "User-Agent": "corplink-rs-managed-routes",
        },
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        data = json.load(response)
    if not isinstance(data, dict):
        raise ValueError("GitHub Meta API response must be an object")
    return data


def fetch_doh(host: str, record_type: str) -> dict:
    url = f"{DEFAULT_DOH_URL}?name={urllib.parse.quote(host)}&type={record_type}"
    request = urllib.request.Request(
        url,
        headers={
            "Accept": "application/dns-json",
            "User-Agent": "corplink-rs-managed-routes",
        },
    )
    with urllib.request.urlopen(request, timeout=30) as response:
        data = json.load(response)
    if not isinstance(data, dict):
        raise ValueError("DoH response must be an object")
    return data


def collect_doh_ips(response: dict, record_type: str) -> list[str]:
    expected_type = {"A": 1, "AAAA": 28}[record_type]
    answers = response.get("Answer") or []
    if not isinstance(answers, list):
        raise ValueError("DoH response Answer must be a list")
    ips: list[str] = []
    for answer in answers:
        if not isinstance(answer, dict) or answer.get("type") != expected_type:
            continue
        data = answer.get("data")
        if not isinstance(data, str):
            continue
        ips.append(str(ipaddress.ip_address(data)))
    return ips


def resolve_github_meta(source: dict, include_ipv6: bool) -> list[str]:
    keys = source.get("keys") or list(DEFAULT_GITHUB_KEYS)
    if not isinstance(keys, list) or not keys:
        raise ValueError("github_meta keys must be a non-empty list")
    meta = fetch_github_meta(source.get("meta_url") or DEFAULT_GITHUB_META_URL)
    routes: list[str] = []
    for key in keys:
        values = meta.get(key)
        if not isinstance(values, list):
            raise ValueError(f"GitHub Meta API response missing list field {key!r}")
        for value in values:
            if not isinstance(value, str):
                raise ValueError(f"GitHub Meta API field {key!r} contains non-string route")
            if not include_ipv6 and ":" in value:
                continue
            routes.append(normalize_route(value))
    return dedupe(routes)


def resolve_dns_hosts(source: dict, include_ipv6: bool) -> list[str]:
    hosts = source.get("hosts")
    if not isinstance(hosts, list) or not hosts:
        raise ValueError("dns_hosts hosts must be a non-empty list")
    routes: list[str] = []
    for host in hosts:
        if not isinstance(host, str) or not host.strip():
            raise ValueError("dns_hosts contains empty host")
        host = host.strip()
        host_routes: list[str] = []
        ips = collect_doh_ips(fetch_doh(host, "A"), "A")
        if include_ipv6:
            ips.extend(collect_doh_ips(fetch_doh(host, "AAAA"), "AAAA"))
        for value in ips:
            ip = ipaddress.ip_address(value)
            if ip.version == 6 and not include_ipv6:
                continue
            if ip.version == 4 and ipaddress.ip_address("198.18.0.0") <= ip <= ipaddress.ip_address("198.19.255.255"):
                print(f"skip fake DNS IP {ip} from dns_hosts {host}", file=sys.stderr)
                continue
            host_routes.append(normalize_route(str(ip)))
        if not host_routes:
            raise ValueError(f"DNS host {host!r} resolved no usable addresses")
        routes.extend(dedupe(host_routes))
    return dedupe(routes)


def resolve_source(source: dict, include_ipv6: bool) -> dict:
    name = source.get("name")
    source_type = source.get("type")
    if not isinstance(name, str) or not name.strip():
        raise ValueError("managed_routes source name must be a non-empty string")
    if source_type == "github_meta":
        routes = resolve_github_meta(source, include_ipv6)
    elif source_type == "dns_hosts":
        routes = resolve_dns_hosts(source, include_ipv6)
    else:
        raise ValueError(f"unsupported managed_routes source type: {source_type!r}")
    if not routes:
        raise ValueError(f"managed_routes source {name!r} returned no routes")
    return {
        "name": name,
        "source_type": source_type,
        "source_fingerprint": source_fingerprint(source, include_ipv6),
        "routes": routes,
        "resolved_at": int(time.time()),
        "error": None,
    }


def source_fingerprint(source: dict, include_ipv6: bool) -> str:
    source_type = source.get("type")
    if source_type == "github_meta":
        material = [
            "v1",
            "github_meta",
            source.get("name"),
            source.get("meta_url") or DEFAULT_GITHUB_META_URL,
            source.get("keys") or list(DEFAULT_GITHUB_KEYS),
            include_ipv6,
        ]
    elif source_type == "dns_hosts":
        material = [
            "v1",
            "dns_hosts",
            source.get("name"),
            source.get("hosts"),
            source.get("port"),
            include_ipv6,
        ]
    else:
        raise ValueError(f"unsupported managed_routes source type: {source_type!r}")
    encoded = json.dumps(material, ensure_ascii=False, separators=(",", ":")).encode()
    return hashlib.sha256(encoded).hexdigest()


def resolve_cache(config: dict) -> dict:
    managed = config.get("managed_routes")
    if not isinstance(managed, dict) or managed.get("enabled") is False:
        return {"version": 1, "sources": []}
    sources = managed.get("sources")
    if not isinstance(sources, list) or not sources:
        raise ValueError("managed_routes.enabled is true but sources is missing or empty")
    include_ipv6 = bool(managed.get("include_ipv6", False))
    return {
        "version": 1,
        "sources": [resolve_source(source, include_ipv6) for source in sources],
    }


def cache_path(config_path: Path, config: dict) -> Path:
    managed = config.get("managed_routes")
    cache_file = DEFAULT_CACHE_FILE
    if isinstance(managed, dict) and isinstance(managed.get("cache_file"), str):
        cache_file = managed["cache_file"]
    path = Path(cache_file)
    if path.is_absolute():
        return path
    return config_path.parent / path


def write_atomic(path: Path, data: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(prefix=f".{path.name}.", suffix=".tmp", dir=path.parent)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as file:
            file.write(data)
        os.replace(tmp_name, path)
    except Exception:
        try:
            os.unlink(tmp_name)
        except FileNotFoundError:
            pass
        raise


def main() -> int:
    args = parse_args()
    try:
        config = load_json(args.config)
        cache = resolve_cache(config)
        output = json.dumps(cache, ensure_ascii=False, indent=2) + "\n"
        if args.write_cache:
            path = cache_path(args.config, config)
            write_atomic(path, output)
            print(f"wrote managed routes cache: {path}", file=sys.stderr)
        else:
            sys.stdout.write(output)
        return 0
    except Exception as error:
        print(f"managed_routes preflight failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())

#!/usr/bin/env python3
"""Create one method-specific AWS SigV4 S3 HEAD query."""

from __future__ import annotations

import argparse
import datetime as dt
import hashlib
import hmac
import os
from urllib.parse import quote


def hmac_sha256(key: bytes, value: str) -> bytes:
    return hmac.new(key, value.encode("utf-8"), hashlib.sha256).digest()


def encoded(value: str) -> str:
    return quote(value, safe="-_.~")


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--authority", required=True)
    parser.add_argument("--bucket", required=True)
    parser.add_argument("--key", required=True)
    parser.add_argument("--access-key")
    parser.add_argument("--secret-key")
    parser.add_argument("--region", default="us-east-1")
    parser.add_argument("--expires", type=int, default=3600)
    parser.add_argument("--time")
    args = parser.parse_args()
    access_key = args.access_key or os.environ.get("PRESIGN_ACCESS_KEY")
    secret_key = args.secret_key or os.environ.get("PRESIGN_SECRET_KEY")
    if not access_key or not secret_key:
        raise SystemExit(
            "credentials require --access-key/--secret-key or the "
            "PRESIGN_ACCESS_KEY/PRESIGN_SECRET_KEY environment"
        )
    if not 1 <= args.expires <= 604800:
        raise SystemExit("expires must be within the SigV4 seven-day limit")

    when = (
        dt.datetime.strptime(args.time, "%Y%m%dT%H%M%SZ").replace(tzinfo=dt.UTC)
        if args.time
        else dt.datetime.now(dt.UTC)
    )
    amz_date = when.strftime("%Y%m%dT%H%M%SZ")
    date = when.strftime("%Y%m%d")
    scope = f"{date}/{args.region}/s3/aws4_request"
    canonical_uri = f"/{encoded(args.bucket)}/{quote(args.key, safe='/-_.~')}"
    parameters = {
        "X-Amz-Algorithm": "AWS4-HMAC-SHA256",
        "X-Amz-Credential": f"{access_key}/{scope}",
        "X-Amz-Date": amz_date,
        "X-Amz-Expires": str(args.expires),
        "X-Amz-SignedHeaders": "host",
    }
    canonical_query = "&".join(
        f"{encoded(name)}={encoded(value)}" for name, value in sorted(parameters.items())
    )
    payload_hash = "UNSIGNED-PAYLOAD"
    canonical_request = "\n".join(
        [
            "HEAD",
            canonical_uri,
            canonical_query,
            f"host:{args.authority}\n",
            "host",
            payload_hash,
        ]
    )
    string_to_sign = "\n".join(
        [
            "AWS4-HMAC-SHA256",
            amz_date,
            scope,
            hashlib.sha256(canonical_request.encode("utf-8")).hexdigest(),
        ]
    )
    date_key = hmac_sha256(f"AWS4{secret_key}".encode(), date)
    region_key = hmac_sha256(date_key, args.region)
    service_key = hmac_sha256(region_key, "s3")
    signing_key = hmac_sha256(service_key, "aws4_request")
    signature = hmac.new(
        signing_key, string_to_sign.encode("utf-8"), hashlib.sha256
    ).hexdigest()
    print(f"{canonical_query}&X-Amz-Signature={signature}")


if __name__ == "__main__":
    main()

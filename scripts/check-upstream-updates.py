#!/usr/bin/env python3
import os
import sys
import json
import urllib.request
import urllib.error

DIGESTS_FILE = ".github/upstream-digests.json"
VERSION_FILE = "VERSION"

IMAGES = {
    "linuxserver/plex:latest": "linuxserver/plex",
    "plexinc/pms-docker:latest": "plexinc/pms-docker"
}

def get_remote_digest(repo, tag="latest"):
    try:
        # Get token
        token_url = f"https://auth.docker.io/token?service=registry.docker.io&scope=repository:{repo}:pull"
        req = urllib.request.Request(token_url)
        with urllib.request.urlopen(req) as response:
            data = json.loads(response.read().decode())
            token = data["token"]
        
        # Get digest using HEAD request first
        manifest_url = f"https://index.docker.io/v2/{repo}/manifests/{tag}"
        req = urllib.request.Request(manifest_url, method="HEAD")
        req.add_header("Authorization", f"Bearer {token}")
        req.add_header("Accept", "application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json")
        
        try:
            with urllib.request.urlopen(req) as response:
                digest = response.headers.get("Docker-Content-Digest")
                if digest:
                    return digest
        except urllib.error.HTTPError:
            # Fallback to GET request if HEAD is not allowed/fails
            pass

        req = urllib.request.Request(manifest_url, method="GET")
        req.add_header("Authorization", f"Bearer {token}")
        req.add_header("Accept", "application/vnd.docker.distribution.manifest.list.v2+json, application/vnd.docker.distribution.manifest.v2+json, application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json")
        with urllib.request.urlopen(req) as response:
            return response.headers.get("Docker-Content-Digest")

    except Exception as e:
        print(f"Error fetching digest for {repo}:{tag}: {e}", file=sys.stderr)
        return None

def bump_patch_version(version_str):
    parts = version_str.strip().split(".")
    if len(parts) == 3:
        try:
            parts[2] = str(int(parts[2]) + 1)
            return ".".join(parts)
        except ValueError:
            pass
    raise ValueError(f"Invalid version format: '{version_str}'")

def main():
    if not os.path.exists(DIGESTS_FILE):
        print(f"Error: {DIGESTS_FILE} not found.", file=sys.stderr)
        sys.exit(1)
        
    if not os.path.exists(VERSION_FILE):
        print(f"Error: {VERSION_FILE} not found.", file=sys.stderr)
        sys.exit(1)

    with open(DIGESTS_FILE, "r") as f:
        current_digests = json.load(f)

    new_digests = {}
    updated = False
    changes = []

    for key, repo in IMAGES.items():
        print(f"Checking remote digest for {key}...")
        remote_digest = get_remote_digest(repo)
        if not remote_digest:
            print(f"Could not retrieve digest for {key}. Skipping.", file=sys.stderr)
            new_digests[key] = current_digests.get(key, "")
            continue
            
        old_digest = current_digests.get(key)
        new_digests[key] = remote_digest
        
        if old_digest != remote_digest:
            print(f"  -> UPDATE DETECTED for {key}!")
            print(f"     Old: {old_digest}")
            print(f"     New: {remote_digest}")
            updated = True
            changes.append(f"upstream image {key} updated")
        else:
            print(f"  -> Up-to-date ({remote_digest[:15]}...)")

    if updated:
        with open(VERSION_FILE, "r") as f:
            old_version = f.read().strip()
            
        try:
            new_version = bump_patch_version(old_version)
        except ValueError as e:
            print(f"Error bumping version: {e}", file=sys.stderr)
            sys.exit(1)

        print(f"Bumping version from {old_version} to {new_version}...")
        
        with open(VERSION_FILE, "w") as f:
            f.write(new_version + "\n")
            
        with open(DIGESTS_FILE, "w") as f:
            json.dump(new_digests, f, indent=2)
            f.write("\n")

        print("Updates saved.")
        
        # Set GitHub Action outputs
        if "GITHUB_OUTPUT" in os.environ:
            with open(os.environ["GITHUB_OUTPUT"], "a") as f:
                f.write(f"updated=true\n")
                f.write(f"version={new_version}\n")
                f.write(f"changes={'; '.join(changes)}\n")
    else:
        print("No updates found.")
        if "GITHUB_OUTPUT" in os.environ:
            with open(os.environ["GITHUB_OUTPUT"], "a") as f:
                f.write("updated=false\n")

if __name__ == "__main__":
    main()

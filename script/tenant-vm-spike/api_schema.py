"""Strict request validation from the pinned published Firecracker 1.17 schema."""

import hashlib
import json
from pathlib import Path
import re

ROOT = Path(__file__).with_name("fixtures")
DIGESTS = {
    "firecracker_spec-v1.17.0.yaml": "473a0a392927b407834b341bf5fbc7925524fedaf9d925e1bbbafb6a1f48c671",
    "firecracker_spec-v1.17.0.json": "29879b2af63084e147f90630f26a11c346a63382d2c7d45708bf8fe90ee702b6",
}
KEYWORDS = {"$ref", "type", "properties", "required", "items", "enum", "minimum", "maximum",
            "exclusiveMinimum", "exclusiveMaximum", "minItems", "maxItems", "uniqueItems",
            "minLength", "maxLength", "pattern", "additionalProperties", "description", "title",
            "default", "format", "readOnly", "example", "deprecated"}


class ApiSchema:
    def __init__(self):
        for name, expected in DIGESTS.items():
            if hashlib.sha256((ROOT / name).read_bytes()).hexdigest() != expected:
                raise ValueError(f"pinned release schema digest mismatch: {name}")
        self.document = json.loads((ROOT / "firecracker_spec-v1.17.0.json").read_text())
        if self.document["info"]["version"] != "1.17.0":
            raise ValueError("wrong Firecracker schema version")

    def validate(self, method, path, body):
        operation = None
        for template, methods in self.document["paths"].items():
            wanted, actual = template.split("/"), path.split("/")
            if len(wanted) == len(actual) and all(
                    a == b or (a.startswith("{") and a.endswith("}") and b)
                    for a, b in zip(wanted, actual)):
                operation = methods.get(method.lower())
                break
        if operation is None:
            raise ValueError(f"not in Firecracker 1.17 API: {method} {path}")
        parameters = [p for p in operation.get("parameters", []) if p.get("in") == "body"]
        if not parameters:
            if body is not None:
                raise ValueError(f"{method} {path} does not accept a body")
            return
        parameter, = parameters
        if body is None:
            if parameter.get("required"):
                raise ValueError(f"{method} {path} requires a body")
            return
        self._value(parameter["schema"], body, path)
        if path == "/snapshot/load" and sum(key in body for key in ("mem_file_path", "mem_backend")) != 1:
            raise ValueError("snapshot load requires exactly one memory source (release schema prose constraint)")

    def _value(self, schema, value, location):
        if "$ref" in schema:
            target = self.document
            reference = schema["$ref"]
            if not reference.startswith("#/"):
                raise ValueError(f"unsupported external schema reference: {reference}")
            for part in reference[2:].split("/"):
                target = target[part.replace("~1", "/").replace("~0", "~")]
            return self._value(target, value, location)
        unknown_keywords = set(schema) - KEYWORDS
        if unknown_keywords:
            raise ValueError(f"unsupported schema keywords at {location}: {sorted(unknown_keywords)}")
        kind = schema.get("type", "object" if "properties" in schema else None)
        types = {"object": lambda v: isinstance(v, dict), "array": lambda v: isinstance(v, list),
                 "string": lambda v: isinstance(v, str), "boolean": lambda v: isinstance(v, bool),
                 "integer": lambda v: isinstance(v, int) and not isinstance(v, bool),
                 "number": lambda v: isinstance(v, (int, float)) and not isinstance(v, bool)}
        if kind is not None and (kind not in types or not types[kind](value)):
            raise ValueError(f"{location} must be {kind}")
        if "enum" in schema and value not in schema["enum"]:
            raise ValueError(f"{location} is outside the declared enum")
        if kind == "object":
            missing = set(schema.get("required", [])) - set(value)
            if missing:
                raise ValueError(f"{location} missing required fields: {sorted(missing)}")
            properties = schema.get("properties", {})
            additional = schema.get("additionalProperties", False)
            for name, item in value.items():
                if name in properties:
                    self._value(properties[name], item, f"{location}.{name}")
                elif isinstance(additional, dict):
                    self._value(additional, item, f"{location}.{name}")
                elif not additional:
                    raise ValueError(f"{location} undeclared request field: {name}")
        if kind == "array":
            if len(value) < schema.get("minItems", 0) or len(value) > schema.get("maxItems", float("inf")):
                raise ValueError(f"{location} array length outside schema bounds")
            for index, item in enumerate(value):
                self._value(schema["items"], item, f"{location}[{index}]")
            if schema.get("uniqueItems") and len({json.dumps(item, sort_keys=True) for item in value}) != len(value):
                raise ValueError(f"{location} requires unique items")
        if kind in ("integer", "number"):
            if "minimum" in schema and (value < schema["minimum"] or
                    (schema.get("exclusiveMinimum") and value == schema["minimum"])):
                raise ValueError(f"{location} below schema minimum")
            if "maximum" in schema and (value > schema["maximum"] or
                    (schema.get("exclusiveMaximum") and value == schema["maximum"])):
                raise ValueError(f"{location} above schema maximum")
        if kind == "string":
            if len(value) < schema.get("minLength", 0) or len(value) > schema.get("maxLength", float("inf")):
                raise ValueError(f"{location} string length outside schema bounds")
            if "pattern" in schema and not re.search(schema["pattern"], value):
                raise ValueError(f"{location} does not match schema pattern")


SCHEMA = ApiSchema()

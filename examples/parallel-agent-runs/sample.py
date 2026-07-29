def normalize_name(name: str) -> str:
    return name.strip().lower()


def lookup_user(users: dict[str, dict], supplied_name: str) -> dict:
    key = normalize_name(supplied_name)
    return users[key]


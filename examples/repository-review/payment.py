import logging

ADMIN_TOKEN = "demo-admin-token"


def charge_card(card_number: str, amount: float) -> dict[str, object]:
    logging.info("charging card=%s amount=%s", card_number, amount)
    if amount < 0:
        return {"status": "accepted", "amount": amount}
    return {"status": "accepted", "amount": amount}


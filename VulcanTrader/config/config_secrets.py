from copy import deepcopy

from VulcanTrader.constants import Config, ExchangeConfig


_SENSITIVE_KEYS = [
    "exchange.key",
    "exchange.api_key",
    "exchange.apiKey",
    "exchange.secret",
    "exchange.password",
    "exchange.uid",
    "exchange.account_id",
    "exchange.accountId",
    "exchange.wallet_address",
    "exchange.walletAddress",
    "exchange.private_key",
    "exchange.privateKey",
    "telegram.token",
    "telegram.chat_id",
    "discord.webhook_url",
    "discord.bot_token",
    "api_server.password",
    "webhook.url",
    "coingecko.api_key",
    # ccxt credentials (requiredCredentials) passed straight through ccxt_config - ccxt also
    # accepts snake_case variants
    *[
        f"exchange.{ccxt_section}.{key}"
        for ccxt_section in ("ccxt_config", "ccxt_sync_config", "ccxt_async_config")
        for key in (
            "apiKey",
            "api_key",
            "secret",
            "uid",
            "accountId",
            "account_id",
            "login",
            "password",
            "twofa",
            "privateKey",
            "private_key",
            "walletAddress",
            "wallet_address",
            "token",
        )
    ],
]


def sanitize_config(config: Config, *, show_sensitive: bool = False) -> Config:
    """
    Remove sensitive information from the config.
    :param config: Configuration
    :param show_sensitive: Show sensitive information
    :return: Configuration
    """
    if show_sensitive:
        return config
    config = deepcopy(config)
    for key in _SENSITIVE_KEYS:
        nested_keys = key.split(".")
        nested_config = _walk(config, nested_keys[:-1])
        if nested_config is not None and nested_config.get(nested_keys[-1]):
            nested_config[nested_keys[-1]] = "REDACTED"

    return config


def _walk(config: dict, path: list[str]) -> dict | None:
    """The nested dict at `path`, or None if any level is missing or not a dict
    (e.g. "ccxt_config": null)."""
    node: object = config
    for part in path:
        if not isinstance(node, dict):
            return None
        node = node.get(part)
    return node if isinstance(node, dict) else None


def remove_exchange_credentials(exchange_config: ExchangeConfig, dry_run: bool) -> None:
    """
    Removes exchange keys from the configuration and specifies dry-run
    Used for backtesting / hyperopt and utils.
    Modifies the input dict!
    :param exchange_config: Exchange configuration
    :param dry_run: If True, remove sensitive keys from the exchange configuration
    """
    if not dry_run:
        return

    # walletAddress / wallet_address must be preserved even in dry-run mode because
    # DEX exchanges (e.g. Hyperliquid) need the address to look up balances and
    # positions via their public read API — it is not an authentication credential.
    _preserve = {"exchange.walletAddress", "exchange.wallet_address"}

    for key in [k for k in _SENSITIVE_KEYS if k.startswith("exchange.")]:
        nested_keys = key.removeprefix("exchange.").split(".")
        if key in _preserve or nested_keys[-1] in ("walletAddress", "wallet_address"):
            continue
        # Walk nested sections too (exchange.ccxt_config.secret, ...) - a credential given
        # straight to ccxt must not survive into dry-run either.
        nested_config = _walk(exchange_config, nested_keys[:-1])
        if nested_config is not None and nested_keys[-1] in nested_config:
            nested_config[nested_keys[-1]] = ""

// SLICE 33: interfaces + dynamic dispatch.
//
// `interface IERC20` is an abstract API surface. A value of
// type `IERC20` carries (at runtime) the name of the module
// it's bound to; dispatch syntax `$value::method(args)` looks
// the bound module up in the loaded set and calls into it.
//
// At compile time, `$value::method(args)` is type-checked
// against the interface declaration: the method must exist,
// the arg types must match. The runtime check at dispatch
// time covers "the bound module isn't loaded" or "the bound
// module's entry fn doesn't exist", with clear errors.
//
// This example: a router that pays through *any* IERC20-shaped
// token. The same `pay_via` body works against either the USD
// or EUR implementations (bound at runtime). The EUR module
// charges a fee internally — the router never knows about it.

module router;

interface IERC20 {
    entry fn transfer(from: Address, to: Address, amount: u64) -> u64;
    entry view fn balance_of(who: Address) -> u64;
}

// The router holds an interface in state — re-binding it
// rotates which token implementation is active.
state token: IERC20;

entry fn use_token(name: string) -> u64 {
    token = IERC20::bind(name);
    return 1u64;
}

// Pay `amount` from `from` to `to` through whatever token is
// currently active. Note the lack of any reference to USD or
// EUR — the router code is implementation-agnostic.
entry fn pay(from: Address, to: Address, amount: u64) -> u64 {
    return $token::transfer(from, to, amount);
}

entry view fn check_balance(who: Address) -> u64 {
    return $token::balance_of(who);
}

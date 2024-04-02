import { ChainId } from './chainId'

type AddressMap = { [chainId: string]: string }

export const KNS_REGISTRY_ADDRESSES: AddressMap = {
    [ChainId.SEPOLIA]: '0x3807fBD692Aa5c96F1D8D7c59a1346a885F40B1C',
    [ChainId.OPTIMISM]: '0xca5b5811c0C40aAB3295f932b1B5112Eb7bb4bD6',
}

export const DOT_OS_ADDRESSES: AddressMap = {
    [ChainId.SEPOLIA]: '0xC5a939923E0B336642024b479502E039338bEd00',
    [ChainId.OPTIMISM]: '0x66929F55Ea1E38591f9430E5013C92cdC01F6cAd',
}

export const NAMEWRAPPER_ADDRESSES: AddressMap = {
    [ChainId.SEPOLIA]: '0x0635513f179D50A207757E05759CbD106d7dFcE8',
    [ChainId.MAINNET]: '0xD4416b13d2b3a9aBae7AcD5D6C2BbDBE25686401',
}

export const ENS_REGISTRY_ADDRESSES: AddressMap = {
    [ChainId.SEPOLIA]: '0x00000000000C2E074eC69A0dFb2997BA6C7d2e1e',
    [ChainId.MAINNET]: '0x00000000000C2E074eC69A0dFb2997BA6C7d2e1e',
}

export const KNS_ENS_ENTRY_ADDRESSES: AddressMap = {
    [ChainId.SEPOLIA]: '0xD4583DFd73B382B7e3230aa29Be774C1843FB7d2',
    [ChainId.GOERLI]: '0xD4583DFd73B382B7e3230aa29Be774C1843FB7d2',
    [ChainId.MAINNET]: '0xa1F47fBBa93574DB4a049C1c5bA03471A21EE01D',
}

export const KNS_ENS_EXIT_ADDRESSES: AddressMap = {
    [ChainId.SEPOLIA]: '0x528bA1BA3186d8CABD2c4E8758a98fAf64eD8Af0',
    [ChainId.OPTIMISM]: '0x0b35664aB5950cE92bce7222be165BB575D9b7c5',
}
import React, { useEffect, useState } from "react";
import { BrowserRouter as Router, Route, Routes } from "react-router-dom";
import { ethers } from "ethers";
import { Web3ReactProvider, Web3ReactHooks } from '@web3-react/core';
import type { MetaMask } from '@web3-react/metamask'

import { PackageStore, PackageStore__factory } from "./abis/types";
import StorePage from "./pages/StorePage";
import MyAppsPage from "./pages/MyAppsPage";
import AppPage from "./pages/AppPage";
import { MY_APPS_PATH } from "./constants/path";
import { ChainId, PACKAGE_STORE_ADDRESSES } from "./constants/chain";
import PublishPage from "./pages/PublishPage";
import { hooks as metaMaskHooks, metaMask } from './utils/metamask'
import "./App.css";

const connectors: [MetaMask, Web3ReactHooks][] = [
  [metaMask, metaMaskHooks],
]

declare global {
  interface ImportMeta {
    env: {
      VITE_SEPOLIA_RPC_URL: string;
      BASE_URL: string;
      VITE_NODE_URL?: string;
      DEV: boolean;
    };
  }
  interface Window {
    our: {
      node: string;
      process: string;
    };
  }
}

const {
  useProvider,
} = metaMaskHooks;

const RPC_URL = import.meta.env.VITE_SEPOLIA_RPC_URL;
const BASE_URL = import.meta.env.BASE_URL;
if (window.our) window.our.process = BASE_URL?.replace("/", "");

const PROXY_TARGET = `${
  import.meta.env.VITE_NODE_URL || "http://localhost:8080"
}${BASE_URL}`;

// This env also has BASE_URL which should match the process + package name
const WEBSOCKET_URL = import.meta.env.DEV // eslint-disable-line
  ? `${PROXY_TARGET.replace("http", "ws")}`
  : undefined;

function App() {
  const provider = useProvider();
  const [nodeConnected, setNodeConnected] = useState(true); // eslint-disable-line

  const [packageAbi, setPackageAbi] = useState<PackageStore>(
    PackageStore__factory.connect(
      PACKAGE_STORE_ADDRESSES[ChainId.SEPOLIA],
      new ethers.providers.JsonRpcProvider(RPC_URL)) // TODO: get the RPC URL from the wallet
  );

  useEffect(() => {
    provider?.getNetwork().then(network => {
      if (network.chainId === ChainId.SEPOLIA) {
        setPackageAbi(PackageStore__factory.connect(
          PACKAGE_STORE_ADDRESSES[ChainId.SEPOLIA],
          provider!.getSigner())
        )
      }
    })
  }, [provider])

  useEffect(() => {
    // if (window.our?.node && window.our?.process) {
    //   const api = new KinodeClientApi({
    //     uri: WEBSOCKET_URL,
    //     nodeId: window.our.node,
    //     processId: window.our.process,
    //     onOpen: (_event, _api) => {
    //       console.log("Connected to Kinode");
    //       // api.send({ data: "Hello World" });
    //     },
    //     onMessage: (json, _api) => {
    //       console.log('UNEXPECTED WEBSOCKET MESSAGE', json)
    //     },
    //   });

    //   setApi(api);
    // } else {
    //   setNodeConnected(false);
    // }
  }, []);

  if (!nodeConnected) {
    return (
      <div className="node-not-connected">
        <h2 style={{ color: "red" }}>Node not connected</h2>
        <h4>
          You need to start a node at {PROXY_TARGET} before you can use this UI
          in development.
        </h4>
      </div>
    );
  }

  const props = { provider, packageAbi };

  return (
    <Web3ReactProvider connectors={connectors}>
      <Router basename={BASE_URL}>
        <Routes>
          <Route path="/" element={<StorePage {...props} />} />
          <Route path={MY_APPS_PATH} element={<MyAppsPage {...props} />} />
          <Route path="/app-details/:id" element={<AppPage {...props} />} />
          <Route path="/publish" element={<PublishPage {...props} />} />
        </Routes>
      </Router>
    </Web3ReactProvider>
  );
}

export default App;

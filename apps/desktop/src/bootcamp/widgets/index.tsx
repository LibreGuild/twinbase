import type { WidgetId } from "../types";
import { BTree, IndexRace, JoinExplorer, Normalize, TableAnatomy } from "./basics";
import { CacheSim, HashRing, LsmVsBtree, Replication, TxIsolation, VectorSpace } from "./systems";

export function Widget({ id }: { id: WidgetId }) {
  switch (id) {
    case "table-anatomy":
      return <TableAnatomy />;
    case "join-explorer":
      return <JoinExplorer />;
    case "index-race":
      return <IndexRace />;
    case "btree":
      return <BTree />;
    case "normalize":
      return <Normalize />;
    case "tx-isolation":
      return <TxIsolation />;
    case "cache-sim":
      return <CacheSim />;
    case "hash-ring":
      return <HashRing />;
    case "replication":
      return <Replication />;
    case "lsm-vs-btree":
      return <LsmVsBtree />;
    case "vector-space":
      return <VectorSpace />;
  }
}

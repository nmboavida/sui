// Copyright (c) Mysten Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

import {
  normalizeSuiAddress,
  SuiAddress,
  SUI_FRAMEWORK_ADDRESS,
} from "@mysten/sui.js";
import { useWalletKit } from "@mysten/wallet-kit";
import { useMutation } from "@tanstack/react-query";
import BigNumber from "bignumber.js";
import provider from "../../../network/provider";
import { SUI_SYSTEM_ID } from "../../../network/queries/sui-system";
import { useMyType } from "../../../network/queries/use-raw";
import { Coin, SUI_COIN } from "../../../network/types";
import { getCoins, getGas } from "../../../utils/coins";
import { StakeButton } from "../../StakeButton";

interface Props {
  validator: SuiAddress;
  /** Amount to Delegate */
  amount: string;
}

const SUI_DECIMALS = 9;
const GAS_BUDGET = 10000n;

function toMist(sui: string) {
  return BigInt(new BigNumber(sui).shiftedBy(SUI_DECIMALS).toString());
}

/**
 * Requests Delegation object for a Validator.
 * Can only be performed if there's no `StakedSui` (hence no `Delegation`) object.
 */
export function AddDelegation({ validator, amount }: Props) {
  const { currentAccount, signAndExecuteTransaction } = useWalletKit();
  const { data: coins } = useMyType<Coin>(SUI_COIN, currentAccount);

  const stakeFor = useMutation(["stake-for-validator"], async () => {
    if (!coins || !coins.length) {
      throw new Error('Not enough coins');
    }

    const mistAmount = toMist(amount);

    const gasPrice = await provider.getReferenceGasPrice();
    const gasRequred = GAS_BUDGET * BigInt(gasPrice);
    const { gas, coins: available, max } = getGas(coins, gasRequred);

    if (mistAmount > max) {
      throw new Error(
        `Requested amount ${mistAmount} is bigger than max ${max}`
      );
    }

    if (!gas) {
      throw new Error('No gas coin found')
    }

    const stakeCoins = getCoins(available, mistAmount);

    await signAndExecuteTransaction({
      kind: "moveCall",
      data: {
        packageObjectId: SUI_FRAMEWORK_ADDRESS,
        module: "sui_system",
        function: "request_add_delegation_mul_coin",
        gasPayment: normalizeSuiAddress(gas.reference.objectId),
        typeArguments: [],
        gasBudget: 10000,
        arguments: [
          SUI_SYSTEM_ID,
          stakeCoins.map((c) => normalizeSuiAddress(c.reference.objectId)),
          [mistAmount.toString()], // Option<u64> // [amt] = Some(amt)
          normalizeSuiAddress(validator),
        ],
      },
    });
  });

  return (
    <StakeButton
      // we can only stake if there's at least 2 coins (one gas and one stake)
      disabled={!amount || !coins?.length || coins.length < 2}
      onClick={() => stakeFor.mutate()}
    >
      Stake
    </StakeButton>
  );
}
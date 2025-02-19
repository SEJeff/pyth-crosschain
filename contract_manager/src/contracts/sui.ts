import {
  Connection,
  Ed25519Keypair,
  JsonRpcProvider,
  ObjectId,
  RawSigner,
  SUI_CLOCK_OBJECT_ID,
  TransactionBlock,
} from "@mysten/sui.js";
import { Chain, SuiChain } from "../chains";
import { DataSource } from "xc_admin_common";
import { Contract } from "../base";

export class SuiContract extends Contract {
  static type = "SuiContract";

  /**
   * Given the ids of the pyth state and wormhole state, create a new SuiContract
   * The package ids are derived based on the state ids
   *
   * @param chain the chain which this contract is deployed on
   * @param stateId id of the pyth state for the deployed contract
   * @param wormholeStateId id of the wormhole state for the wormhole contract that pyth binds to
   */
  constructor(
    public chain: SuiChain,
    public stateId: string,
    public wormholeStateId: string
  ) {
    super();
  }

  static fromJson(chain: Chain, parsed: any): SuiContract {
    if (parsed.type !== SuiContract.type) throw new Error("Invalid type");
    if (!(chain instanceof SuiChain))
      throw new Error(`Wrong chain type ${chain}`);
    return new SuiContract(chain, parsed.stateId, parsed.wormholeStateId);
  }

  getType(): string {
    return SuiContract.type;
  }

  getChain(): SuiChain {
    return this.chain;
  }

  toJson() {
    return {
      chain: this.chain.getId(),
      stateId: this.stateId,
      wormholeStateId: this.wormholeStateId,
      type: SuiContract.type,
    };
  }

  /**
   * Given a objectId, returns the id for the package that the object belongs to.
   * @param objectId
   */
  async getPackageId(objectId: ObjectId): Promise<ObjectId> {
    const provider = this.getProvider();
    const state = await provider
      .getObject({
        id: objectId,
        options: {
          showContent: true,
        },
      })
      .then((result) => {
        if (result.data?.content?.dataType == "moveObject") {
          return result.data.content.fields;
        }

        throw new Error("not move object");
      });

    if ("upgrade_cap" in state) {
      return state.upgrade_cap.fields.package;
    }

    throw new Error("upgrade_cap not found");
  }

  async getPythPackageId(): Promise<ObjectId> {
    return await this.getPackageId(this.stateId);
  }

  async getWormholePackageId(): Promise<ObjectId> {
    return await this.getPackageId(this.wormholeStateId);
  }

  getId(): string {
    return `${this.chain.getId()}_${this.stateId}`;
  }

  /**
   * Fetches the price table object id for the current state id
   */
  async getPriceTableId(): Promise<ObjectId> {
    const provider = this.getProvider();
    let result = await provider.getDynamicFieldObject({
      parentId: this.stateId,
      name: {
        type: "vector<u8>",
        value: "price_info",
      },
    });
    if (!result.data) {
      throw new Error("Price Table not found, contract may not be initialized");
    }
    return result.data.objectId;
  }

  private async parsePrice(priceInfo: any) {
    const packageId = await this.getPythPackageId();
    const expectedType = `${packageId}::price::Price`;
    if (priceInfo.type !== expectedType) {
      throw new Error(
        `Price type mismatch, expected ${expectedType} but found ${priceInfo.type}`
      );
    }
    let expo = priceInfo.fields.expo.fields.magnitude;
    if (priceInfo.fields.expo.fields.negative) expo = "-" + expo;
    let price = priceInfo.fields.price.fields.magnitude;
    if (priceInfo.fields.price.fields.negative) price = "-" + price;
    return {
      conf: priceInfo.fields.conf,
      publishTime: priceInfo.fields.timestamp,
      expo,
      price,
    };
  }

  async getPriceFeed(feedId: string) {
    const tableId = await this.getPriceTableId();
    const provider = this.getProvider();
    let result = await provider.getDynamicFieldObject({
      parentId: tableId,
      name: {
        type: `${await this.getPythPackageId()}::price_identifier::PriceIdentifier`,
        value: {
          bytes: Array.from(Buffer.from(feedId, "hex")),
        },
      },
    });
    if (!result.data || !result.data.content) {
      return undefined;
    }
    if (result.data.content.dataType !== "moveObject") {
      throw new Error("Price feed type mismatch");
    }
    let priceInfoObjectId = result.data.content.fields.value;
    let priceInfo = await provider.getObject({
      id: priceInfoObjectId,
      options: { showContent: true },
    });
    if (!priceInfo.data || !priceInfo.data.content) {
      throw new Error(
        `Price feed ID ${priceInfoObjectId} in price table but object not found!!`
      );
    }
    if (priceInfo.data.content.dataType !== "moveObject") {
      throw new Error(
        `Expected ${priceInfoObjectId} to be a moveObject (PriceInfoObject)`
      );
    }
    return {
      emaPrice: await this.parsePrice(
        priceInfo.data.content.fields.price_info.fields.price_feed.fields
          .ema_price
      ),
      price: await this.parsePrice(
        priceInfo.data.content.fields.price_info.fields.price_feed.fields.price
      ),
    };
  }

  /**
   * Given a signed VAA, execute the migration instruction on the pyth contract.
   * The payload of the VAA can be obtained from the `getUpgradePackagePayload` method.
   * @param vaa
   * @param keypair used to sign the transaction
   */
  async executeMigrateInstruction(vaa: Buffer, keypair: Ed25519Keypair) {
    const tx = new TransactionBlock();
    const packageId = await this.getPythPackageId();
    let decreeReceipt = await this.getVaaDecreeReceipt(tx, packageId, vaa);

    tx.moveCall({
      target: `${packageId}::migrate::migrate`,
      arguments: [tx.object(this.stateId), decreeReceipt],
    });

    return this.executeTransaction(tx, keypair);
  }

  async executeUpdatePriceFeed(senderPrivateKey: string, vaas: Buffer[]) {
    throw new Error("Not implemented");
  }

  async executeGovernanceInstruction(senderPrivateKey: string, vaa: Buffer) {
    const keypair = Ed25519Keypair.fromSecretKey(
      Buffer.from(senderPrivateKey, "hex")
    );
    const tx = new TransactionBlock();
    const packageId = await this.getPythPackageId();
    let decreeReceipt = await this.getVaaDecreeReceipt(tx, packageId, vaa);

    tx.moveCall({
      target: `${packageId}::governance::execute_governance_instruction`,
      arguments: [tx.object(this.stateId), decreeReceipt],
    });

    return this.executeTransaction(tx, keypair);
  }

  async executeUpgradeInstruction(
    vaa: Buffer,
    keypair: Ed25519Keypair,
    modules: number[][],
    dependencies: string[]
  ) {
    const tx = new TransactionBlock();
    const packageId = await this.getPythPackageId();
    let decreeReceipt = await this.getVaaDecreeReceipt(tx, packageId, vaa);

    const [upgradeTicket] = tx.moveCall({
      target: `${packageId}::contract_upgrade::authorize_upgrade`,
      arguments: [tx.object(this.stateId), decreeReceipt],
    });

    const [upgradeReceipt] = tx.upgrade({
      modules,
      dependencies,
      packageId: packageId,
      ticket: upgradeTicket,
    });

    tx.moveCall({
      target: `${packageId}::contract_upgrade::commit_upgrade`,
      arguments: [tx.object(this.stateId), upgradeReceipt],
    });
    return this.executeTransaction(tx, keypair);
  }

  /**
   * Utility function to get the decree receipt object for a VAA that can be
   * used to authorize a governance instruction.
   * @param tx
   * @param packageId pyth package id
   * @param vaa
   * @private
   */
  private async getVaaDecreeReceipt(
    tx: TransactionBlock,
    packageId: string,
    vaa: Buffer
  ) {
    const wormholePackageId = await this.getWormholePackageId();
    let [decreeTicket] = tx.moveCall({
      target: `${packageId}::set_update_fee::authorize_governance`,
      arguments: [tx.object(this.stateId), tx.pure(false)],
    });

    let [verifiedVAA] = tx.moveCall({
      target: `${wormholePackageId}::vaa::parse_and_verify`,
      arguments: [
        tx.object(this.wormholeStateId),
        tx.pure(Array.from(vaa)),
        tx.object(SUI_CLOCK_OBJECT_ID),
      ],
    });

    let [decreeReceipt] = tx.moveCall({
      target: `${wormholePackageId}::governance_message::verify_vaa`,
      arguments: [tx.object(this.wormholeStateId), verifiedVAA, decreeTicket],
      typeArguments: [`${packageId}::governance_witness::GovernanceWitness`],
    });
    return decreeReceipt;
  }

  /**
   * Given a transaction block and a keypair, sign and execute it
   * Sets the gas budget to 2x the estimated gas cost
   * @param tx
   * @param keypair
   * @private
   */
  private async executeTransaction(
    tx: TransactionBlock,
    keypair: Ed25519Keypair
  ) {
    const provider = this.getProvider();
    let txBlock = {
      transactionBlock: tx,
      options: {
        showEffects: true,
        showEvents: true,
      },
    };
    const wallet = new RawSigner(keypair, provider);
    let gasCost = await wallet.getGasCostEstimation(txBlock);
    tx.setGasBudget(gasCost * BigInt(2));
    return wallet.signAndExecuteTransactionBlock(txBlock);
  }

  async getValidTimePeriod() {
    const fields = await this.getStateFields();
    return Number(fields.stale_price_threshold);
  }

  async getDataSources(): Promise<DataSource[]> {
    const provider = this.getProvider();
    let result = await provider.getDynamicFieldObject({
      parentId: this.stateId,
      name: {
        type: "vector<u8>",
        value: "data_sources",
      },
    });
    if (!result.data || !result.data.content) {
      throw new Error(
        "Data Sources not found, contract may not be initialized"
      );
    }
    if (result.data.content.dataType !== "moveObject") {
      throw new Error("Data Sources type mismatch");
    }
    return result.data.content.fields.value.fields.keys.map(
      ({ fields }: any) => {
        return {
          emitterChain: Number(fields.emitter_chain),
          emitterAddress: Buffer.from(
            fields.emitter_address.fields.value.fields.data
          ).toString("hex"),
        };
      }
    );
  }

  async getGovernanceDataSource(): Promise<DataSource> {
    const fields = await this.getStateFields();
    const governanceFields = fields.governance_data_source.fields;
    const chainId = governanceFields.emitter_chain;
    const emitterAddress =
      governanceFields.emitter_address.fields.value.fields.data;
    return {
      emitterChain: Number(chainId),
      emitterAddress: Buffer.from(emitterAddress).toString("hex"),
    };
  }

  async getBaseUpdateFee() {
    const fields = await this.getStateFields();
    return { amount: fields.base_update_fee };
  }

  async getLastExecutedGovernanceSequence() {
    const fields = await this.getStateFields();
    return Number(fields.last_executed_governance_sequence);
  }

  private getProvider() {
    return new JsonRpcProvider(new Connection({ fullnode: this.chain.rpcUrl }));
  }

  private async getStateFields() {
    const provider = this.getProvider();
    const result = await provider.getObject({
      id: this.stateId,
      options: { showContent: true },
    });
    if (
      !result.data ||
      !result.data.content ||
      result.data.content.dataType !== "moveObject"
    )
      throw new Error("Unable to fetch pyth state object");
    return result.data.content.fields;
  }
}

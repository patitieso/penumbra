use anyhow::Result;
use async_trait::async_trait;
use ibc_types::core::{
    ics02_client::{client_state::ClientState, consensus_state::ConsensusState},
    ics03_connection::{
        connection::{ConnectionEnd, Counterparty, State},
        msgs::conn_open_confirm::MsgConnectionOpenConfirm,
    },
    ics24_host::path::ConnectionPath,
};
use penumbra_chain::component::PENUMBRA_COMMITMENT_PREFIX;
use penumbra_storage::{StateRead, StateWrite};

use crate::{
    component::{
        client::StateReadExt as _,
        connection::{StateReadExt as _, StateWriteExt as _},
        MsgHandler,
    },
    event,
};

#[async_trait]
impl MsgHandler for MsgConnectionOpenConfirm {
    async fn check_stateless(&self) -> Result<()> {
        // NOTE: other than that the message is a well formed ConnectionOpenConfirm,
        // there is no other stateless validation to perform.

        Ok(())
    }

    async fn try_execute<S: StateWrite>(&self, mut state: S) -> Result<()> {
        tracing::debug!(msg = ?self);
        // Validate a ConnectionOpenConfirm message, completing the IBC connection handshake.
        //
        // Verify that we have a connection in the correct state (TRYOPEN), and that the
        // counterparty has committed a connection with the expected state (OPEN) to their state
        // store.
        //
        // Here we are Chain B.
        // CHAINS:          (A, B)
        // PRIOR STATE:     (OPEN, TRYOPEN)
        // POSTERIOR STATE: (OPEN, OPEN)
        //
        // verify that a connection with the provided ID exists and is in the correct state
        // (TRYOPEN)
        let connection = verify_previous_connection(&state, self).await?;

        let expected_conn = ConnectionEnd::new(
            State::Open,
            connection.counterparty().client_id().clone(),
            Counterparty::new(
                connection.client_id().clone(),
                Some(self.conn_id_on_b.clone()),
                PENUMBRA_COMMITMENT_PREFIX.clone(),
            ),
            connection.versions().to_vec(),
            connection.delay_period(),
        );

        // get the trusted client state for the counterparty
        let trusted_client_state = state.get_client_state(connection.client_id()).await?;

        // check if the client is frozen
        // TODO: should we also check if the client is expired here?
        if trusted_client_state.is_frozen() {
            return Err(anyhow::anyhow!("client is frozen"));
        }

        // get the stored consensus state for the counterparty
        let trusted_consensus_state = state
            .get_verified_consensus_state(self.proof_height_on_a, connection.client_id().clone())
            .await?;

        // PROOF VERIFICATION
        // in connectionOpenConfirm, only the inclusion of the connection state must be
        // verified, not the client or consensus states.
        trusted_client_state.verify_connection_state(
            self.proof_height_on_a,
            connection.counterparty().prefix(),
            &self.proof_conn_end_on_a,
            trusted_consensus_state.root(),
            &ConnectionPath::new(
                connection
                    .counterparty()
                    .connection_id()
                    .ok_or_else(|| anyhow::anyhow!("invalid counterparty"))?,
            ),
            &expected_conn,
        )?;

        // VERIFICATION SUCCESSFUL. now execute
        let mut connection = state
            .get_connection(&self.conn_id_on_b)
            .await?
            .ok_or_else(|| anyhow::anyhow!("no connection with the given ID"))
            .unwrap();

        connection.set_state(State::Open);

        state.update_connection(&self.conn_id_on_b, connection.clone());

        state.record(event::connection_open_confirm(
            &self.conn_id_on_b,
            &connection,
        ));

        Ok(())
    }
}

async fn verify_previous_connection<S: StateRead>(
    state: S,
    msg: &MsgConnectionOpenConfirm,
) -> anyhow::Result<ConnectionEnd> {
    let connection = state
        .get_connection(&msg.conn_id_on_b)
        .await?
        .ok_or_else(|| anyhow::anyhow!("connection not found"))?;

    if !connection.state_matches(&State::TryOpen) {
        return Err(anyhow::anyhow!("connection not in correct state"));
    } else {
        Ok(connection)
    }
}

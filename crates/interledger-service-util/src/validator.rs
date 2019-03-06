use chrono::{DateTime, Duration, Utc};
use futures::{future::err, Future};
use hex;
use interledger_packet::{ErrorCode, RejectBuilder};
use interledger_service::*;
use ring::digest::{digest, SHA256};
use std::marker::PhantomData;
use std::time::Duration as StdDuration;
use tokio::prelude::FutureExt;

#[derive(Clone)]
pub struct ValidatorService<S, A> {
    next: S,
    account_type: PhantomData<A>,
}

impl<S, A> ValidatorService<S, A>
where
    S: IncomingService<A>,
    A: Account,
{
    pub fn incoming(next: S) -> Self {
        ValidatorService {
            next,
            account_type: PhantomData,
        }
    }
}

impl<S, A> ValidatorService<S, A>
where
    S: OutgoingService<A>,
    A: Account,
{
    pub fn outgoing(next: S) -> Self {
        ValidatorService {
            next,
            account_type: PhantomData,
        }
    }
}

impl<S, A> IncomingService<A> for ValidatorService<S, A>
where
    S: IncomingService<A>,
    A: Account,
{
    type Future = BoxedIlpFuture;

    fn handle_request(&mut self, request: IncomingRequest<A>) -> Self::Future {
        let expires_at = DateTime::from(request.prepare.expires_at());
        if expires_at >= Utc::now() {
            Box::new(self.next.handle_request(request))
        } else {
            error!(
                "Incoming packet expired {}ms ago at {} (time now: {})",
                (Utc::now() - expires_at).num_milliseconds(),
                expires_at.to_rfc3339(),
                Utc::now().to_rfc3339()
            );
            let result = Box::new(err(RejectBuilder {
                code: ErrorCode::R00_TRANSFER_TIMED_OUT,
                message: &[],
                triggered_by: &[],
                data: &[],
            }
            .build()));
            Box::new(result)
        }
    }
}

impl<S, A> OutgoingService<A> for ValidatorService<S, A>
where
    S: OutgoingService<A>,
    A: Account,
{
    type Future = BoxedIlpFuture;

    fn send_request(&mut self, request: OutgoingRequest<A>) -> Self::Future {
        let mut condition: [u8; 32] = [0; 32];
        condition[..].copy_from_slice(request.prepare.execution_condition());

        let time_left = DateTime::from(request.prepare.expires_at()) - Utc::now();
        if time_left > Duration::seconds(0) {
            Box::new(
                self.next
                    .send_request(request)
                    .timeout(time_left.to_std().unwrap_or(StdDuration::from_secs(30)))
                    .map_err(move |err| {
                        // If the error was caused by the timer, into_inner will return None
                        if let Some(reject) = err.into_inner() {
                            reject
                        } else {
                            error!(
                                "Outgoing request timed out after {}ms",
                                time_left.num_milliseconds()
                            );
                            RejectBuilder {
                                code: ErrorCode::R00_TRANSFER_TIMED_OUT,
                                message: &[],
                                triggered_by: &[],
                                data: &[],
                            }
                            .build()
                        }
                    })
                    .and_then(move |fulfill| {
                        let generated_condition = digest(&SHA256, fulfill.fulfillment());
                        if generated_condition.as_ref() == condition {
                            Ok(fulfill)
                        } else {
                            error!("Fulfillment did not match condition. Fulfillment: {}, hash: {}, actual condition: {}", hex::encode(fulfill.fulfillment()), hex::encode(generated_condition), hex::encode(condition));
                            Err(RejectBuilder {
                                code: ErrorCode::F09_INVALID_PEER_RESPONSE,
                                message: b"Fulfillment did not match condition",
                                triggered_by: &[],
                                data: &[],
                            }
                            .build())
                        }
                    }),
            )
        } else {
            error!(
                "Outgoing packet expired {}ms ago",
                time_left.num_milliseconds()
            );
            // Already expired
            Box::new(err(RejectBuilder {
                code: ErrorCode::R00_TRANSFER_TIMED_OUT,
                message: &[],
                triggered_by: &[],
                data: &[],
            }
            .build()))
        }
    }
}

#[cfg(test)]
mod incoming_tests {
    use super::*;
    use interledger_packet::*;
    use interledger_test_helpers::*;
    use std::time::SystemTime;

    #[test]
    fn lets_through_valid_incoming_packet() {
        let test = TestIncomingService::fulfill(
            FulfillBuilder {
                fulfillment: &[0; 32],
                data: b"test data",
            }
            .build(),
        );
        let mut validator = ValidatorService::incoming(test.clone());
        let result = validator
            .handle_request(IncomingRequest {
                from: TestAccount::new(0),
                prepare: PrepareBuilder {
                    destination: b"example.destination",
                    amount: 100,
                    expires_at: SystemTime::from(Utc::now() + Duration::seconds(30)),
                    execution_condition: &[
                        102, 104, 122, 173, 248, 98, 189, 119, 108, 143, 193, 139, 142, 159, 142,
                        32, 8, 151, 20, 133, 110, 226, 51, 179, 144, 42, 89, 29, 13, 95, 41, 37,
                    ],
                    data: b"test data",
                }
                .build(),
            })
            .wait();

        assert_eq!(test.get_incoming_requests().len(), 1);
        assert!(result.is_ok());
    }

    #[test]
    fn rejects_expired_incoming_packet() {
        let test = TestIncomingService::fulfill(
            FulfillBuilder {
                fulfillment: &[0; 32],
                data: b"test data",
            }
            .build(),
        );
        let mut validator = ValidatorService::incoming(test.clone());
        let result = validator
            .handle_request(IncomingRequest {
                from: TestAccount::new(0),
                prepare: PrepareBuilder {
                    destination: b"example.destination",
                    amount: 100,
                    expires_at: SystemTime::from(Utc::now() - Duration::seconds(30)),
                    execution_condition: &[
                        102, 104, 122, 173, 248, 98, 189, 119, 108, 143, 193, 139, 142, 159, 142,
                        32, 8, 151, 20, 133, 110, 226, 51, 179, 144, 42, 89, 29, 13, 95, 41, 37,
                    ],
                    data: b"test data",
                }
                .build(),
            })
            .wait();

        assert_eq!(test.get_incoming_requests().len(), 0);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().code(),
            ErrorCode::R00_TRANSFER_TIMED_OUT
        );
    }
}

#[cfg(test)]
mod outgoing_tests {
    use super::*;
    use interledger_packet::*;
    use interledger_test_helpers::*;
    use std::time::SystemTime;

    #[test]
    fn lets_through_valid_outgoing_response() {
        let test = TestOutgoingService::fulfill(
            FulfillBuilder {
                fulfillment: &[0; 32],
                data: b"test data",
            }
            .build(),
        );
        let mut validator = ValidatorService::outgoing(test.clone());
        let result = validator
            .send_request(OutgoingRequest {
                from: TestAccount::new(0),
                to: TestAccount::new(1),
                prepare: PrepareBuilder {
                    destination: b"example.destination",
                    amount: 100,
                    expires_at: SystemTime::from(Utc::now() + Duration::seconds(30)),
                    execution_condition: &[
                        102, 104, 122, 173, 248, 98, 189, 119, 108, 143, 193, 139, 142, 159, 142,
                        32, 8, 151, 20, 133, 110, 226, 51, 179, 144, 42, 89, 29, 13, 95, 41, 37,
                    ],
                    data: b"test data",
                }
                .build(),
            })
            .wait();

        assert_eq!(test.get_outgoing_requests().len(), 1);
        assert!(result.is_ok());
    }

    #[test]
    fn returns_reject_instead_of_invalid_fulfillment() {
        let test = TestOutgoingService::fulfill(
            FulfillBuilder {
                fulfillment: &[6; 32],
                data: b"test data",
            }
            .build(),
        );
        let mut validator = ValidatorService::outgoing(test.clone());
        let result = validator
            .send_request(OutgoingRequest {
                from: TestAccount::new(0),
                to: TestAccount::new(1),
                prepare: PrepareBuilder {
                    destination: b"example.destination",
                    amount: 100,
                    expires_at: SystemTime::from(Utc::now() + Duration::seconds(30)),
                    execution_condition: &[
                        102, 104, 122, 173, 248, 98, 189, 119, 108, 143, 193, 139, 142, 159, 142,
                        32, 8, 151, 20, 133, 110, 226, 51, 179, 144, 42, 89, 29, 13, 95, 41, 37,
                    ],
                    data: b"test data",
                }
                .build(),
            })
            .wait();

        assert_eq!(test.get_outgoing_requests().len(), 1);
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().code(),
            ErrorCode::F09_INVALID_PEER_RESPONSE
        );
    }
}

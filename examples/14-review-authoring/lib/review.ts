import type { ReviewPacket } from '../.atomic/types/schemas/review-packet';

export const reviewLabel = (packet: ReviewPacket): string => `Change ${packet.change_id}`;

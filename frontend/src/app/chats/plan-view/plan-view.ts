import { Component, input } from '@angular/core';
import { MatCardModule } from '@angular/material/card';
import { MatIconModule } from '@angular/material/icon';
import type { PlanEntry } from '../../core/api/types';

@Component({
  selector: 'hub-plan-view',
  imports: [MatCardModule, MatIconModule],
  templateUrl: './plan-view.html',
  styleUrl: './plan-view.scss',
})
export class PlanViewComponent {
  readonly entries = input<PlanEntry[]>([]);
  readonly heading = input('Execution plan');

  statusIcon(status: string): string {
    if (status === 'completed') return 'check_circle';
    if (status === 'in_progress') return 'progress_activity';
    return 'radio_button_unchecked';
  }
}

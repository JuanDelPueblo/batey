import { Component, OnInit, inject } from '@angular/core';
import { MatButtonModule } from '@angular/material/button';
import { MatIconModule } from '@angular/material/icon';
import { MatTooltipModule } from '@angular/material/tooltip';
import { RouterLink } from '@angular/router';
import { RegistryBrowserComponent } from '../../agents/registry-browser/registry-browser';
import { AppStateService } from '../../state/app-state.service';

@Component({
  selector: 'hub-agent-registry-page',
  imports: [MatButtonModule, MatIconModule, MatTooltipModule, RouterLink, RegistryBrowserComponent],
  templateUrl: './agent-registry-page.html',
  styleUrl: './agent-registry-page.scss',
})
export class AgentRegistryPageComponent implements OnInit {
  readonly state = inject(AppStateService);

  ngOnInit(): void {
    void this.state.loadRegistry();
    void this.state.loadOperations();
  }
}
